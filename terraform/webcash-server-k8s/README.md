# webcash-server-k8s

Terraform module to deploy webycash-server with Redis and FoundationDB on any Kubernetes cluster.

## Supported Providers

| Provider | Variable | Authentication |
|----------|----------|----------------|
| **GCP GKE** | `cloud_provider = "gcp"` | `gcp_project`, `gcp_region` |
| **AWS EKS** | `cloud_provider = "aws"` | `aws_region`, `aws_cluster_name` |
| **Azure AKS** | `cloud_provider = "azure"` | `azure_resource_group`, `azure_cluster_name` |
| **DigitalOcean** | `cloud_provider = "digitalocean"` | `digitalocean_cluster_id` |
| **OVH** | `cloud_provider = "ovh"` | `kubeconfig_path` |
| **Generic** | `cloud_provider = "generic"` | `kubeconfig_path`, `kubeconfig_context` |

## Architecture

```
                    ┌─── Ingress (nginx) ───┐
                    │                       │
              ┌─────▼─────┐          ┌──────▼──────┐
              │  server-0  │          │  server-1   │   (HPA: 2-8 replicas)
              └─────┬──────┘          └──────┬──────┘
                    │                        │
         ┌──────────┴────────────────────────┘
         │
    ┌────▼─────┐        ┌──────────────────┐
    │  Redis   │◄──────►│  FoundationDB    │
    │ Sentinel │        │  (StatefulSet)   │
    │ (3 pods) │        │  (3 pods)        │
    └──────────┘        └──────────────────┘
```

## Usage

### Testnet on generic Kubernetes

```hcl
module "webcash_testnet" {
  source = "./terraform/webcash-server-k8s"

  environment    = "testnet"
  db_backend     = "redis"
  server_version = "v0.1.0"
  cloud_provider = "generic"
  kubeconfig_path = "~/.kube/config"
}
```

### Production on GKE with Redis+FDB

```hcl
module "webcash_prod" {
  source = "./terraform/webcash-server-k8s"

  environment     = "production"
  db_backend      = "redis_fdb"
  server_replicas = 4
  server_version  = "v0.1.0"

  cloud_provider = "gcp"
  gcp_project    = "my-project"
  gcp_region     = "us-central1"

  enable_ingress = true
  ingress_host   = "webcash.example.com"

  mining_difficulty  = 20
  mining_amount_wats = 20000000000
}
```

### Production on AWS EKS

```hcl
module "webcash_prod" {
  source = "./terraform/webcash-server-k8s"

  environment      = "production"
  db_backend       = "redis_fdb"
  cloud_provider   = "aws"
  aws_region       = "us-east-1"
  aws_cluster_name = "my-eks-cluster"
}
```

## Inputs

See [variables.tf](variables.tf) for all available inputs.

## Outputs

| Output | Description |
|--------|-------------|
| `namespace` | Kubernetes namespace created |
| `server_service` | ClusterIP service name |
| `server_endpoint` | Internal HTTP endpoint |
| `ingress_host` | External hostname (if enabled) |
| `db_backend` | Active database backend |
| `environment` | Deployment environment |
