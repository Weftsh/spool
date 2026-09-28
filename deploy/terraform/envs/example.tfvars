# One environment's settings. Copy this file to envs/<env>.tfvars — the
# file name is the environment, `scripts/tf.sh <env> …` reads it — and
# keep `env` below equal to that name. Nothing secret belongs here: the
# credentials GitHub issues live in Secrets Manager (docs/operations.md),
# and this file only says whether to use them.
#
#   cp envs/example.tfvars envs/prod.tfvars
#   scripts/tf.sh prod plan

env        = "prod"
aws_region = "us-east-1"

# Serve on a name of your own. Leave both empty to start on CloudFront's
# generated hostname and add a name later (docs/deployment-aws.md).
# parent_domain_name only when that zone is in this same AWS account.
# domain_name        = "git.example.com"
# parent_domain_name = "example.com"

# Two tasks survive losing one; autoscaling adds more on CPU and requests.
desired_count = 2
max_count     = 10

# Leave on for anything you would mind losing. Off makes `destroy` clean
# for a trial stack: no final snapshot, buckets and repositories emptied,
# secrets deleted at once.
deletion_protection = true

# Set once the <project>/<env>/github-app secret is filled
# (docs/operations.md, "GitHub App"); the plan refuses a blank one.
# github_app_slug = "acme-spool"

# Anything else the server reads (docs/operations.md, "Configuration
# reference"), as plain values or from Secrets Manager.
# extra_environment = {
#   STRATUM_LAND_POLL_SECS = "5"
# }
# extra_secrets = {
#   STRATUM_MAIL_SMTP_PASSWORD = "arn:aws:secretsmanager:us-east-1:111111111111:secret:smtp-password-AbCdEf"
# }
