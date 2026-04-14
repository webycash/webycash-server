output "namespace" {
  description = "Kubernetes namespace"
  value       = kubernetes_namespace.webcash.metadata[0].name
}

output "server_service" {
  description = "webycash-server ClusterIP service name"
  value       = kubernetes_service.server.metadata[0].name
}

output "server_endpoint" {
  description = "Internal endpoint for the webycash-server"
  value       = "http://${kubernetes_service.server.metadata[0].name}.${var.namespace}.svc.cluster.local"
}

output "ingress_host" {
  description = "External hostname (if ingress enabled)"
  value       = var.enable_ingress && var.ingress_host != "" ? var.ingress_host : "none"
}

output "db_backend" {
  description = "Active database backend"
  value       = var.db_backend
}

output "environment" {
  description = "Deployment environment"
  value       = var.environment
}
