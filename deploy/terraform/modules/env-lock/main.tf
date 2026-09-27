# The state remembers which environment it is.
#
# Every environment applies from the one root module. The state is chosen
# at `init` (`-backend-config=key=<project>/<env>.tfstate`) and the
# environment at `plan` (`-var-file=envs/<env>.tfvars`), and nothing ties
# the two flags together — a directory whose `.terraform/` was initialised
# for prod happily plans `envs/test.tfvars` against prod's state. That plan
# is not a rehearsal: it renames every `stratum-prod-*` resource to
# `stratum-test-*`, which for most of them is destroy-and-create, and the
# protections that key on `var.env` — deletion protection, `force_delete`,
# the secrets' recovery window — all read "test" by then and stand aside.
#
# So the first apply writes the environment into the state, and every
# later plan refuses a `var.env` that disagrees, before a single resource
# is touched. `ignore_changes` is what makes the stored value stick: the
# input may drift, the recorded output does not, and the precondition
# compares the two. `scripts/tf.sh` sets both flags from one argument so
# the mismatch is hard to produce; this is what stops it when it is.

variable "env" { type = string }

resource "terraform_data" "env" {
  input = var.env
  lifecycle {
    ignore_changes = [input]
  }
}

resource "terraform_data" "lock" {
  input = var.env
  lifecycle {
    precondition {
      condition     = terraform_data.env.output == var.env
      error_message = "This state belongs to env \"${terraform_data.env.output}\" but the plan was given env \"${var.env}\": the backend key and the tfvars point at different environments. Re-run through scripts/tf.sh <env>, or `terraform init -reconfigure -backend-config=key=<project>/${var.env}.tfstate`."
    }
  }
}

output "env" {
  description = "The environment the state was first applied as."
  value       = terraform_data.env.output
}
