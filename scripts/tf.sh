#!/bin/sh
# Terraform for one environment, with the state and the variables chosen
# by the same word.
#
#   AWS_PROFILE=weft-tf scripts/tf.sh prod plan
#   AWS_PROFILE=weft-tf scripts/tf.sh test apply
#   AWS_PROFILE=weft-tf scripts/tf.sh test destroy
#   AWS_PROFILE=weft-tf scripts/tf.sh prod output name_servers
#
# `deploy/terraform` is one root module for every environment: the state
# key is fixed at `init` and the tfvars at `plan`, and typed by hand they
# can disagree — `envs/test.tfvars` against prod's state is a plan to
# rename production. This re-initialises the backend for the named
# environment before every command and passes its tfvars to the commands
# that take them, so the pair cannot be mixed here; `modules/env-lock`
# refuses the mix if it is produced some other way.
#
# The state bucket is the bootstrap stack's output (TF_STATE_BUCKET to
# skip reading it); the region is the environment's own, from its tfvars,
# the same value deploy.yml uses for both the backend and the stack.
set -eu

env=${1:?environment: prod, test}
shift
[ $# -gt 0 ] || { echo "usage: scripts/tf.sh <env> <terraform command> [args]" >&2; exit 2; }

root=$(cd "$(dirname "$0")/.." && pwd)
dir=$root/deploy/terraform
vars=envs/$env.tfvars
[ -f "$dir/$vars" ] || { echo "no $dir/$vars: the environment is the name of its tfvars file" >&2; exit 2; }

project=$(awk -F'"' '/^variable "project"/{p=1} p&&/default/{print $2; exit}' "$dir/variables.tf")
bucket=${TF_STATE_BUCKET:-$(terraform -chdir="$dir/bootstrap" output -raw state_bucket)}
region=$(awk -F'"' '/^aws_region *=/{print $2; exit}' "$dir/$vars")
[ -n "$region" ] || { echo "$vars does not set aws_region" >&2; exit 2; }
export AWS_REGION="$region"

terraform -chdir="$dir" init -input=false -reconfigure \
  -backend-config="bucket=$bucket" \
  -backend-config="key=$project/$env.tfstate" \
  -backend-config="region=$region" \
  -backend-config="use_lockfile=true" > /dev/null

cmd=$1
shift
case $cmd in
  plan|apply|destroy|refresh|import|console) set -- -var-file="$vars" "$@" ;;
esac
exec terraform -chdir="$dir" "$cmd" "$@"
