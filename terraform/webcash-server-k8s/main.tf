terraform {
  required_version = ">= 1.5"
  required_providers {
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = ">= 2.25"
    }
    helm = {
      source  = "hashicorp/helm"
      version = ">= 2.12"
    }
  }
}

# ── Provider configuration (adapts to cloud_provider) ───────────────

provider "kubernetes" {
  config_path    = var.cloud_provider == "generic" ? var.kubeconfig_path : null
  config_context = var.kubeconfig_context != "" ? var.kubeconfig_context : null
}

provider "helm" {
  kubernetes {
    config_path    = var.cloud_provider == "generic" ? var.kubeconfig_path : null
    config_context = var.kubeconfig_context != "" ? var.kubeconfig_context : null
  }
}

# ── Namespace ───────────────────────────────────────────────────────

resource "kubernetes_namespace" "webcash" {
  metadata {
    name = var.namespace
    labels = {
      app         = "webycash-server"
      environment = var.environment
    }
  }
}

# ── ConfigMap: server configuration ─────────────────────────────────

resource "kubernetes_config_map" "server_config" {
  metadata {
    name      = "webycash-server-config"
    namespace = kubernetes_namespace.webcash.metadata[0].name
  }

  data = {
    "config.toml" = templatefile("${path.module}/templates/config.toml.tpl", {
      environment       = var.environment
      db_backend        = var.db_backend
      mining_difficulty  = var.mining_difficulty
      mining_amount_wats = var.mining_amount_wats
      redis_host        = "redis-master.${var.namespace}.svc.cluster.local"
      fdb_cluster_file  = "/etc/foundationdb/fdb.cluster"
    })
  }
}

# ── Redis (via Helm chart — HA with Sentinel) ───────────────────────

resource "helm_release" "redis" {
  count = var.db_backend == "redis" || var.db_backend == "redis_fdb" ? 1 : 0

  name       = "redis"
  namespace  = kubernetes_namespace.webcash.metadata[0].name
  repository = "https://charts.bitnami.com/bitnami"
  chart      = "redis"
  version    = "19.6.4"

  set {
    name  = "architecture"
    value = "replication"
  }
  set {
    name  = "replica.replicaCount"
    value = var.redis_replicas
  }
  set {
    name  = "sentinel.enabled"
    value = "true"
  }
  set {
    name  = "master.persistence.size"
    value = var.redis_storage_size
  }
  set {
    name  = "replica.persistence.size"
    value = var.redis_storage_size
  }
  dynamic "set" {
    for_each = var.storage_class != "" ? [1] : []
    content {
      name  = "global.storageClass"
      value = var.storage_class
    }
  }
  set {
    name  = "auth.enabled"
    value = "false"
  }
}

# ── FoundationDB (StatefulSet) ──────────────────────────────────────

resource "kubernetes_stateful_set" "fdb" {
  count = var.db_backend == "foundationdb" || var.db_backend == "redis_fdb" ? 1 : 0

  metadata {
    name      = "foundationdb"
    namespace = kubernetes_namespace.webcash.metadata[0].name
    labels = {
      app = "foundationdb"
    }
  }

  spec {
    replicas     = var.fdb_replicas
    service_name = "foundationdb"

    selector {
      match_labels = {
        app = "foundationdb"
      }
    }

    template {
      metadata {
        labels = {
          app = "foundationdb"
        }
      }

      spec {
        container {
          name  = "fdb"
          image = "foundationdb/foundationdb:7.3.43"

          port {
            container_port = 4500
          }

          env {
            name  = "FDB_NETWORKING_MODE"
            value = "container"
          }

          volume_mount {
            name       = "fdb-data"
            mount_path = "/var/fdb/data"
          }

          volume_mount {
            name       = "fdb-config"
            mount_path = "/etc/foundationdb"
          }

          resources {
            requests = {
              cpu    = "500m"
              memory = "512Mi"
            }
            limits = {
              cpu    = "2"
              memory = "2Gi"
            }
          }
        }

        init_container {
          name  = "fdb-init"
          image = "foundationdb/foundationdb:7.3.43"
          command = ["/bin/bash", "-c", <<-EOT
            FDB_CLUSTER="webcash:webcash@foundationdb-0.foundationdb.${var.namespace}.svc.cluster.local:4500"
            echo "$FDB_CLUSTER" > /etc/foundationdb/fdb.cluster
            if [ "$(hostname)" = "foundationdb-0" ]; then
              fdbcli -C /etc/foundationdb/fdb.cluster --exec "configure new single ssd" 2>/dev/null || true
            fi
          EOT
          ]

          volume_mount {
            name       = "fdb-config"
            mount_path = "/etc/foundationdb"
          }
        }

        volume {
          name = "fdb-config"
          empty_dir {}
        }
      }
    }

    volume_claim_template {
      metadata {
        name = "fdb-data"
      }
      spec {
        access_modes = ["ReadWriteOnce"]
        resources {
          requests = {
            storage = var.fdb_storage_size
          }
        }
        dynamic "storage_class_name" {
          for_each = var.storage_class != "" ? [1] : []
          content {
            storage_class_name = var.storage_class
          }
        }
      }
    }
  }
}

# ── FDB headless service ────────────────────────────────────────────

resource "kubernetes_service" "fdb" {
  count = var.db_backend == "foundationdb" || var.db_backend == "redis_fdb" ? 1 : 0

  metadata {
    name      = "foundationdb"
    namespace = kubernetes_namespace.webcash.metadata[0].name
  }

  spec {
    selector = {
      app = "foundationdb"
    }
    cluster_ip = "None"
    port {
      port        = 4500
      target_port = 4500
    }
  }
}

# ── webycash-server Deployment (HA) ─────────────────────────────────

resource "kubernetes_deployment" "server" {
  metadata {
    name      = "webycash-server"
    namespace = kubernetes_namespace.webcash.metadata[0].name
    labels = {
      app         = "webycash-server"
      environment = var.environment
    }
  }

  spec {
    replicas = var.server_replicas

    selector {
      match_labels = {
        app = "webycash-server"
      }
    }

    strategy {
      type = "RollingUpdate"
      rolling_update {
        max_surge       = "1"
        max_unavailable = "0"
      }
    }

    template {
      metadata {
        labels = {
          app         = "webycash-server"
          environment = var.environment
        }
        annotations = var.enable_monitoring ? {
          "prometheus.io/scrape" = "true"
          "prometheus.io/port"   = "8080"
          "prometheus.io/path"   = "/api/v1/stats"
        } : {}
      }

      spec {
        container {
          name  = "server"
          image = "${var.server_image}:${var.server_version}"

          port {
            container_port = 8080
            name           = "http"
          }

          env {
            name  = "WEBCASH_MODE"
            value = var.environment == "production" ? "production" : "testnet"
          }
          env {
            name  = "WEBCASH_DB_BACKEND"
            value = var.db_backend
          }
          env {
            name  = "WEBCASH_DIFFICULTY"
            value = tostring(var.mining_difficulty)
          }
          env {
            name  = "WEBCASH_MINING_AMOUNT"
            value = tostring(var.mining_amount_wats)
          }
          env {
            name  = "WEBCASH_SUBSIDY_AMOUNT"
            value = "0"
          }
          env {
            name  = "WEBCASH_BIND_ADDR"
            value = "0.0.0.0:8080"
          }
          env {
            name  = "REDIS_URL"
            value = "redis://redis-master.${var.namespace}.svc.cluster.local:6379"
          }
          env {
            name  = "RUST_LOG"
            value = "info"
          }

          volume_mount {
            name       = "config"
            mount_path = "/etc/webycash"
          }

          readiness_probe {
            http_get {
              path = "/api/v1/health"
              port = 8080
            }
            initial_delay_seconds = 5
            period_seconds        = 10
          }

          liveness_probe {
            http_get {
              path = "/api/v1/health"
              port = 8080
            }
            initial_delay_seconds = 10
            period_seconds        = 30
          }

          resources {
            requests = {
              cpu    = "250m"
              memory = "128Mi"
            }
            limits = {
              cpu    = "1"
              memory = "512Mi"
            }
          }
        }

        volume {
          name = "config"
          config_map {
            name = kubernetes_config_map.server_config.metadata[0].name
          }
        }
      }
    }
  }
}

# ── Server Service (LoadBalancer) ───────────────────────────────────

resource "kubernetes_service" "server" {
  metadata {
    name      = "webycash-server"
    namespace = kubernetes_namespace.webcash.metadata[0].name
    labels = {
      app = "webycash-server"
    }
  }

  spec {
    selector = {
      app = "webycash-server"
    }
    type = "ClusterIP"
    port {
      port        = 80
      target_port = 8080
      name        = "http"
    }
  }
}

# ── Ingress (optional) ──────────────────────────────────────────────

resource "kubernetes_ingress_v1" "server" {
  count = var.enable_ingress && var.ingress_host != "" ? 1 : 0

  metadata {
    name      = "webycash-server"
    namespace = kubernetes_namespace.webcash.metadata[0].name
    annotations = {
      "kubernetes.io/ingress.class" = "nginx"
    }
  }

  spec {
    rule {
      host = var.ingress_host
      http {
        path {
          path      = "/"
          path_type = "Prefix"
          backend {
            service {
              name = kubernetes_service.server.metadata[0].name
              port {
                number = 80
              }
            }
          }
        }
      }
    }
  }
}

# ── Horizontal Pod Autoscaler ───────────────────────────────────────

resource "kubernetes_horizontal_pod_autoscaler_v2" "server" {
  metadata {
    name      = "webycash-server"
    namespace = kubernetes_namespace.webcash.metadata[0].name
  }

  spec {
    scale_target_ref {
      api_version = "apps/v1"
      kind        = "Deployment"
      name        = kubernetes_deployment.server.metadata[0].name
    }

    min_replicas = var.server_replicas
    max_replicas = var.server_replicas * 4

    metric {
      type = "Resource"
      resource {
        name = "cpu"
        target {
          type                = "Utilization"
          average_utilization = 70
        }
      }
    }
  }
}
