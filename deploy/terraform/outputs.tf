output "name_servers" {
  description = "With a domain configured: the four name servers the registrar must delegate to. Nothing else in the stack finishes until it does."
  value       = var.domain_name == "" ? [] : module.dns[0].name_servers
}

output "sites_name_servers" {
  description = "With a sites domain configured: the four name servers its registrar must delegate to. Until it does, the sites certificate stays PENDING_VALIDATION and nothing can be served on that name."
  value       = var.sites_domain_name == "" ? [] : module.sites[0].name_servers
}

output "base_url" {
  description = "Where the product lives: the custom domain, or CloudFront's hostname without one."
  value       = module.app.base_url
}

output "alb_url" {
  description = "Direct ALB endpoint (plain HTTP) — for pathological >60s operations that outrun CloudFront's origin read timeout."
  value       = module.app.alb_url
}

output "ssh_endpoint" {
  description = "git-over-SSH endpoint (NLB, TCP 22)."
  value       = module.app.ssh_endpoint
}

output "ecr_repository_url" {
  value = module.app.ecr_repository_url
}

output "cluster_name" {
  value = module.app.cluster_name
}

output "service_name" {
  value = module.app.service_name
}

output "store_bucket" {
  value = module.data.store_bucket
}

output "runner_cluster_name" {
  value = module.runner.cluster_name
}

output "runner_task_definition" {
  value = module.runner.task_definition_arn
}

output "runner_ecr_repository_url" {
  value = module.runner.ecr_repository_url
}

output "github_runner_task_definition" {
  value = module.runner.github_task_definition_arn
}

output "github_runner_ecr_repository_url" {
  value = module.runner.github_ecr_repository_url
}
