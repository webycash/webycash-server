# webcash-server-k8s Terraform Module
# Deploys webycash-server + Redis + FoundationDB on any Kubernetes cluster
# Supports: GCP GKE, AWS EKS, Azure AKS, DigitalOcean DOKS, OVH, generic k8s

variable "namespace" {
  description = "Kubernetes namespace for the deployment"
  type        = string
  default     = "webcash"
}

variable "environment" {
  description = "Deployment environment: testnet or production"
  type        = string
  default     = "testnet"
  validation {
    condition     = contains(["testnet", "production"], var.environment)
    error_message = "environment must be 'testnet' or 'production'"
  }
}

variable "server_version" {
  description = "webycash-server Docker image tag (release version)"
  type        = string
  default     = "v0.1.0"
}

variable "server_replicas" {
  description = "Number of webycash-server replicas (HA)"
  type        = number
  default     = 2
}

variable "server_image" {
  description = "webycash-server container image (override for custom registries)"
  type        = string
  default     = "ghcr.io/webycash/webycash-server"
}

# Database backend selection
variable "db_backend" {
  description = "Database backend: redis, foundationdb, or redis_fdb"
  type        = string
  default     = "redis_fdb"
  validation {
    condition     = contains(["redis", "foundationdb", "redis_fdb"], var.db_backend)
    error_message = "db_backend must be 'redis', 'foundationdb', or 'redis_fdb'"
  }
}

# Redis configuration
variable "redis_replicas" {
  description = "Number of Redis replicas (HA via Redis Sentinel)"
  type        = number
  default     = 3
}

variable "redis_storage_size" {
  description = "Redis persistent storage size"
  type        = string
  default     = "10Gi"
}

# FoundationDB configuration
variable "fdb_replicas" {
  description = "Number of FoundationDB storage processes"
  type        = number
  default     = 3
}

variable "fdb_storage_size" {
  description = "FoundationDB storage size per pod"
  type        = string
  default     = "50Gi"
}

# Mining configuration
variable "mining_difficulty" {
  description = "Mining difficulty (bits). Testnet: 16, Production: dynamic"
  type        = number
  default     = 16
}

variable "mining_amount_wats" {
  description = "Mining reward in wats (8 decimal places). 20000000000 = 200.00000000"
  type        = number
  default     = 20000000000
}

# Kubernetes provider credentials
variable "kubeconfig_path" {
  description = "Path to kubeconfig file (for generic k8s providers)"
  type        = string
  default     = "~/.kube/config"
}

variable "kubeconfig_context" {
  description = "Kubeconfig context to use"
  type        = string
  default     = ""
}

# Cloud provider (for managed k8s)
variable "cloud_provider" {
  description = "Cloud provider: gcp, aws, azure, digitalocean, ovh, or generic"
  type        = string
  default     = "generic"
  validation {
    condition     = contains(["gcp", "aws", "azure", "digitalocean", "ovh", "generic"], var.cloud_provider)
    error_message = "cloud_provider must be one of: gcp, aws, azure, digitalocean, ovh, generic"
  }
}

# Cloud-specific credentials (optional, used when cloud_provider != generic)
variable "gcp_project" {
  description = "GCP project ID (for GKE)"
  type        = string
  default     = ""
}

variable "gcp_region" {
  description = "GCP region (for GKE)"
  type        = string
  default     = "us-central1"
}

variable "aws_region" {
  description = "AWS region (for EKS)"
  type        = string
  default     = "us-east-1"
}

variable "aws_cluster_name" {
  description = "EKS cluster name"
  type        = string
  default     = ""
}

variable "azure_resource_group" {
  description = "Azure resource group (for AKS)"
  type        = string
  default     = ""
}

variable "azure_cluster_name" {
  description = "AKS cluster name"
  type        = string
  default     = ""
}

variable "digitalocean_cluster_id" {
  description = "DigitalOcean Kubernetes cluster ID"
  type        = string
  default     = ""
}

variable "enable_monitoring" {
  description = "Enable Prometheus metrics scraping"
  type        = bool
  default     = true
}

variable "enable_ingress" {
  description = "Create an Ingress resource for external access"
  type        = bool
  default     = true
}

variable "ingress_host" {
  description = "Hostname for the Ingress (e.g., testnet.weby.cash)"
  type        = string
  default     = ""
}

variable "storage_class" {
  description = "Kubernetes StorageClass name (empty = default)"
  type        = string
  default     = ""
}
