output "name_servers" {
  description = "With a domain configured: the four name servers the parent zone must delegate `domain_name` to. The certificate — and so the full apply — waits until it does."
  value       = var.domain_name == "" ? [] : module.dns[0].name_servers
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
  description = "Where to push the server image. The service runs `<this>:<image_tag>`."
  value       = module.app.ecr_repository_url
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

output "github_app_secret_name" {
  description = "The Secrets Manager secret to fill with your GitHub App's credentials before setting github_app_slug (docs/operations.md)."
  value       = module.data.github_app_secret_name
}
