terraform {
  # 1.10 is where the S3 backend learned to lock on the state key itself
  # (`use_lockfile`); before that it needed a DynamoDB table.
  required_version = ">= 1.10"

  # Values come from `terraform init -backend-config` (the deploy workflow
  # supplies the bucket from a repository variable and `use_lockfile=true`;
  # local runs pass the bootstrap outputs). Empty here so credential-free `init -backend=false`
  # + `validate` works in CI.
  backend "s3" {}

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
    tls = {
      source  = "hashicorp/tls"
      version = "~> 4.0"
    }
  }
}

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = {
      Project     = var.project
      Environment = var.env
      ManagedBy   = "terraform"
    }
  }
}

# CloudFront accepts certificates from us-east-1 only. The DNS module
# issues its certificate through this alias so a stack in any other
# region still gets one CloudFront will take; in us-east-1 it is the same
# region twice, which is harmless.
provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"

  default_tags {
    tags = {
      Project     = var.project
      Environment = var.env
      ManagedBy   = "terraform"
    }
  }
}
