# The lock's whole contract, against a real terraform: the first apply
# records the env, the same env plans clean, a different env is refused
# before anything is planned. Runs offline — terraform_data is built in.

run "first_apply_records_the_env" {
  command = apply
  variables { env = "prod" }
  assert {
    condition     = output.env == "prod"
    error_message = "the first apply did not record its env"
  }
}

run "the_same_env_plans_clean" {
  command = plan
  variables { env = "prod" }
  assert {
    condition     = output.env == "prod"
    error_message = "a plan with the recorded env changed it"
  }
}

run "a_different_env_is_refused" {
  command = plan
  variables { env = "test" }
  expect_failures = [terraform_data.lock]
}

run "the_recorded_env_survives_the_refused_plan" {
  command = plan
  variables { env = "prod" }
  assert {
    condition     = output.env == "prod"
    error_message = "the refused plan moved the recorded env"
  }
}
