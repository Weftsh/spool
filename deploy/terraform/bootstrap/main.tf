# One-time bootstrap, applied by an operator with local state BEFORE the
# root stack or any CI deploy can run. It creates the things the pipeline
# itself depends on: remote state storage, and the GitHub-OIDC roles CI
# assumes (no long-lived AWS keys anywhere).
#
#   cd deploy/terraform/bootstrap
#   terraform init && terraform apply -var github_repo=OWNER/REPO
#
# Outputs feed GitHub repository variables (see docs/deployment-aws.md).

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
  type    = string
  default = "stratum"
}

variable "github_repo" {
  description = "GitHub repository (owner/name) allowed to assume the CI roles."
  type        = string
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
  github_owner = split("/", var.github_repo)[0]
  github_name  = split("/", var.github_repo)[1]

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
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
  # GitHub's OIDC root CA thumbprint; AWS now verifies against its own
  # trust store and treats this as informational, but the field is required.
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]
}

# stratum-cd: the narrow role every main-branch deploy assumes — push an
# image, roll the service, run the smoke bootstrap task. Deliberately NO
# secretsmanager access: secrets flow to tasks via the execution role.
data "aws_iam_policy_document" "cd_trust" {
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.github.arn]
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
  name               = "${var.project}-cd"
  assume_role_policy = data.aws_iam_policy_document.cd_trust.json
}

data "aws_iam_policy_document" "cd" {
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
  name   = "deploy"
  role   = aws_iam_role.cd.id
  policy = data.aws_iam_policy_document.cd.json
}

# stratum-infra: the broad role terraform applies run under. Its trust
# additionally requires the GitHub environment "infra" — put required
# reviewers on that environment and every infra change gets a human gate.
data "aws_iam_policy_document" "infra_trust" {
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.github.arn]
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
  name               = "${var.project}-infra"
  assume_role_policy = data.aws_iam_policy_document.infra_trust.json
}

resource "aws_iam_role_policy_attachment" "infra_admin" {
  role       = aws_iam_role.infra.name
  policy_arn = "arn:aws:iam::aws:policy/AdministratorAccess"
}

# ------------------------------------------------------------------ outputs

output "state_bucket" {
  value = aws_s3_bucket.tf_state.bucket
}

output "cd_role_arn" {
  value = aws_iam_role.cd.arn
}

output "infra_role_arn" {
  value = aws_iam_role.infra.arn
}
