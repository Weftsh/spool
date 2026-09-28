# One-time bootstrap, applied by an operator with local state BEFORE the
# root stack. It creates what the root stack's own state depends on — the
# S3 bucket that holds it — and, optionally, two roles a GitHub Actions
# workflow in YOUR repository can assume through OIDC to deploy, so no
# long-lived AWS key has to live in GitHub.
#
#   cd deploy/terraform/bootstrap
#   terraform init && terraform apply                              # state bucket only
#   terraform init && terraform apply -var github_repo=OWNER/REPO  # + the CI roles
#
# Keep this directory's local state (terraform.tfstate) somewhere safe;
# see docs/deployment-aws.md.

terraform {
  required_version = ">= 1.10"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

variable "aws_region" {
  type    = string
  default = "us-east-1"
}

variable "project" {
  description = "Must match the root stack's `project`: the CD role's grants are scoped to `<project>-*` names."
  type        = string
  default     = "spool"
}

variable "github_repo" {
  description = "GitHub repository (owner/name) whose workflows may assume the CI roles — a push to its `main` branch for the deploy role, a job in its `infra` environment for the terraform role. Empty = no GitHub OIDC provider and no roles: the state bucket only."
  type        = string
  default     = ""

  validation {
    condition     = var.github_repo == "" || can(regex("^[^/@]+/[^/@]+$", var.github_repo))
    error_message = "github_repo is owner/name, e.g. acme/spool-deploy."
  }
}

variable "github_oidc_provider_arn" {
  description = "An account holds one OIDC provider per issuer URL. If this account already has one for token.actions.githubusercontent.com (another project made it), pass its ARN here and it is reused instead of created — creating a second fails with EntityAlreadyExists."
  type        = string
  default     = ""
}

# GitHub is migrating the OIDC `sub` claim to immutable identifiers: the
# owner and repository names now carry their numeric ids, so a push to main
# arrives as `repo:owner@6254949/name@1341182177:ref:refs/heads/main` rather
# than `repo:owner/name:ref:refs/heads/main`. The rollout is not settled —
# the repository's own sub-customization API reports the id-bearing prefix
# while still reporting `use_immutable_subject: false` — so trust both
# spellings rather than betting on either. The names are still matched
# exactly; only the ids are wildcarded, and neither an owner nor a repository
# name may contain `@` or `/`, so no other repository can mint a claim that
# matches. When this stopped matching the error was
# `Not authorized to perform sts:AssumeRoleWithWebIdentity`, which reads as a
# broken bootstrap; the `sub` actually presented is in CloudTrail — see
# docs/deployment-aws.md.
locals {
  github_enabled = var.github_repo != ""
  github_owner   = local.github_enabled ? split("/", var.github_repo)[0] : ""
  github_name    = local.github_enabled ? split("/", var.github_repo)[1] : ""

  # Both spellings, per claim suffix.
  cd_subs = [
    "repo:${local.github_owner}/${local.github_name}:ref:refs/heads/main",
    "repo:${local.github_owner}@*/${local.github_name}@*:ref:refs/heads/main",
  ]
  infra_subs = [
    "repo:${local.github_owner}/${local.github_name}:environment:infra",
    "repo:${local.github_owner}@*/${local.github_name}@*:environment:infra",
  ]
}

data "aws_caller_identity" "current" {}

# ---------------------------------------------------------------- state

resource "aws_s3_bucket" "tf_state" {
  bucket = "${var.project}-tf-state-${data.aws_caller_identity.current.account_id}"
}

resource "aws_s3_bucket_versioning" "tf_state" {
  bucket = aws_s3_bucket.tf_state.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_public_access_block" "tf_state" {
  bucket                  = aws_s3_bucket.tf_state.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# No lock table. The S3 backend locks with a conditional write on
# `<key>.tflock` beside the state (`use_lockfile`, terraform >= 1.10) —
# the same conditional-PUT primitive the store rests on for I9 — so the
# DynamoDB table every older runbook reaches for is a second service to
# create, permit and pay for that holds nothing but a mutex.

# ------------------------------------------------------------- GitHub OIDC

resource "aws_iam_openid_connect_provider" "github" {
  count          = local.github_enabled && var.github_oidc_provider_arn == "" ? 1 : 0
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
  # GitHub's OIDC root CA thumbprint; AWS now verifies against its own
  # trust store and treats this as informational, but the field is required.
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]
}

locals {
  github_oidc_provider_arn = var.github_oidc_provider_arn != "" ? var.github_oidc_provider_arn : one(aws_iam_openid_connect_provider.github[*].arn)
}

# <project>-cd: the narrow role a main-branch deploy assumes — push an
# image, roll the service, run a one-off admin task. Deliberately NO
# secretsmanager access: secrets flow to tasks via the execution role.
data "aws_iam_policy_document" "cd_trust" {
  count = local.github_enabled ? 1 : 0
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [local.github_oidc_provider_arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values   = local.cd_subs
    }
  }
}

resource "aws_iam_role" "cd" {
  count              = local.github_enabled ? 1 : 0
  name               = "${var.project}-cd"
  assume_role_policy = data.aws_iam_policy_document.cd_trust[0].json
}

data "aws_iam_policy_document" "cd" {
  count = local.github_enabled ? 1 : 0
  statement {
    sid       = "EcrAuth"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }
  statement {
    sid = "EcrPush"
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:BatchGetImage",
      "ecr:CompleteLayerUpload",
      "ecr:GetDownloadUrlForLayer",
      "ecr:InitiateLayerUpload",
      "ecr:PutImage",
      "ecr:UploadLayerPart",
    ]
    resources = ["arn:aws:ecr:${var.aws_region}:${data.aws_caller_identity.current.account_id}:repository/${var.project}-*"]
  }
  statement {
    sid = "EcsDeploy"
    actions = [
      "ecs:DescribeClusters",
      "ecs:DescribeServices",
      "ecs:DescribeTaskDefinition",
      "ecs:DescribeTasks",
      "ecs:ListTasks",
      "ecs:RegisterTaskDefinition",
      "ecs:RunTask",
      "ecs:UpdateService",
    ]
    resources = ["*"]
  }
  statement {
    sid       = "PassEcsRoles"
    actions   = ["iam:PassRole"]
    resources = ["arn:aws:iam::${data.aws_caller_identity.current.account_id}:role/${var.project}-*"]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["ecs-tasks.amazonaws.com"]
    }
  }
  statement {
    sid       = "ReadDeployParams"
    actions   = ["ssm:GetParameter", "ssm:GetParameters", "ssm:GetParametersByPath"]
    resources = ["arn:aws:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter/${var.project}/*"]
  }
  statement {
    sid = "ReadSmokeLogs"
    actions = [
      "logs:GetLogEvents",
      "logs:DescribeLogStreams",
    ]
    resources = ["arn:aws:logs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:log-group:/${var.project}/*"]
  }
}

resource "aws_iam_role_policy" "cd" {
  count  = local.github_enabled ? 1 : 0
  name   = "deploy"
  role   = aws_iam_role.cd[0].id
  policy = data.aws_iam_policy_document.cd[0].json
}

# <project>-infra: the broad role terraform applies run under. Its trust
# additionally requires the GitHub environment "infra" — put required
# reviewers on that environment and every infra change gets a human gate.
data "aws_iam_policy_document" "infra_trust" {
  count = local.github_enabled ? 1 : 0
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [local.github_oidc_provider_arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    # StringLike, not StringEquals, only so the immutable-id spelling above
    # can be matched: the wildcards stand in for the numeric owner and
    # repository ids and nothing else. `environment:infra` is still exact,
    # so the human gate on that environment is unchanged.
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values   = local.infra_subs
    }
  }
}

resource "aws_iam_role" "infra" {
  count              = local.github_enabled ? 1 : 0
  name               = "${var.project}-infra"
  assume_role_policy = data.aws_iam_policy_document.infra_trust[0].json
}

resource "aws_iam_role_policy_attachment" "infra_admin" {
  count      = local.github_enabled ? 1 : 0
  role       = aws_iam_role.infra[0].name
  policy_arn = "arn:aws:iam::aws:policy/AdministratorAccess"
}

# ------------------------------------------------------------------ outputs

output "state_bucket" {
  value = aws_s3_bucket.tf_state.bucket
}

output "cd_role_arn" {
  description = "Empty without github_repo."
  value       = one(aws_iam_role.cd[*].arn)
}

output "infra_role_arn" {
  description = "Empty without github_repo."
  value       = one(aws_iam_role.infra[*].arn)
}
