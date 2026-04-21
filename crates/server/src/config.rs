use serde::Deserialize;
use std::path::Path;

/// Root configuration — three pluggable backend axes.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub mining: MiningConfig,
    #[serde(default)]
    pub compute: ComputeConfig,
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub mode: NetworkMode,
    pub bind_addr: String,
    pub db: DbConfig,
    #[serde(default)]
    pub cors_origin: Option<String>,
    #[serde(default)]
    pub h2: Option<H2Config>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct H2Config {
    pub max_concurrent_streams: Option<u32>,
    pub initial_window_size: Option<u32>,
    pub max_frame_size: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Testnet,
    Production,
}

// ── Database backends ────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct DbConfig {
    pub backend: DbBackend,
    pub redis_url: Option<String>,
    pub dynamodb_endpoint: Option<String>,
    pub fdb_cluster_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DbBackend {
    Redis,
    DynamoDb,
    FoundationDb,
    #[serde(rename = "redis_fdb")]
    RedisFdb,
}

// ── Compute backends ─────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ComputeConfig {
    /// Which compute backend to use: cpu, cuda, wgpu
    #[serde(default = "default_compute_backend")]
    pub backend: ComputeBackendType,
    /// Minimum batch size to dispatch to GPU (below this, use CPU)
    #[serde(default = "default_gpu_threshold")]
    pub gpu_batch_threshold: usize,
}

impl Default for ComputeConfig {
    fn default() -> Self {
        Self {
            backend: ComputeBackendType::Cpu,
            gpu_batch_threshold: 1000,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ComputeBackendType {
    Cpu,
    Cuda,
    Wgpu,
    /// Auto-detect: try CUDA first, then wgpu, fallback to CPU
    Auto,
}

fn default_compute_backend() -> ComputeBackendType {
    ComputeBackendType::Auto
}
fn default_gpu_threshold() -> usize {
    1000
}

// ── Network plane ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// Network plane: kernel (default) or dpdk (AF_XDP)
    #[serde(default = "default_network_plane")]
    pub plane: NetworkPlane,
    /// AF_XDP interface name (required when plane = dpdk)
    pub dpdk_iface: Option<String>,
    /// AF_XDP UDS path for CNI integration
    pub dpdk_dp_path: Option<String>,
    /// Use pinned BPF map instead of UDS
    #[serde(default)]
    pub dpdk_use_pinned_map: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            plane: NetworkPlane::Kernel,
            dpdk_iface: None,
            dpdk_dp_path: None,
            dpdk_use_pinned_map: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPlane {
    /// Standard kernel TCP/IP stack (default, works everywhere)
    Kernel,
    /// AF_XDP / DPDK user-space networking (Linux only, requires setup)
    Dpdk,
}

fn default_network_plane() -> NetworkPlane {
    NetworkPlane::Kernel
}

// ── Mining ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MiningConfig {
    pub testnet_difficulty: u32,
    pub initial_difficulty: u32,
    pub reports_per_epoch: u64,
    pub target_epoch_seconds: u64,
    pub initial_mining_amount_wats: i64,
    pub initial_subsidy_amount_wats: i64,
}

// ── Loading ──────────────────────────────────────────────────────────

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let mode = match std::env::var("WEBCASH_MODE")
            .unwrap_or_else(|_| "testnet".into())
            .as_str()
        {
            "production" => NetworkMode::Production,
            _ => NetworkMode::Testnet,
        };

        let backend = match std::env::var("WEBCASH_DB_BACKEND")
            .unwrap_or_else(|_| "redis".into())
            .as_str()
        {
            "dynamodb" => DbBackend::DynamoDb,
            "foundationdb" => DbBackend::FoundationDb,
            "redis_fdb" => DbBackend::RedisFdb,
            _ => DbBackend::Redis,
        };

        let compute_backend = match std::env::var("WEBCASH_COMPUTE")
            .unwrap_or_else(|_| "auto".into())
            .as_str()
        {
            "cpu" => ComputeBackendType::Cpu,
            "cuda" => ComputeBackendType::Cuda,
            "wgpu" => ComputeBackendType::Wgpu,
            _ => ComputeBackendType::Auto,
        };

        let network_plane = match std::env::var("WEBCASH_NETWORK")
            .unwrap_or_else(|_| "kernel".into())
            .as_str()
        {
            "dpdk" => NetworkPlane::Dpdk,
            _ => NetworkPlane::Kernel,
        };

        let difficulty: u32 = std::env::var("WEBCASH_DIFFICULTY")
            .unwrap_or_else(|_| "16".into())
            .parse()?;
        let mining_amount: i64 = std::env::var("WEBCASH_MINING_AMOUNT")
            .unwrap_or_else(|_| "20000000000".into())
            .parse()?;
        let subsidy_amount: i64 = std::env::var("WEBCASH_SUBSIDY_AMOUNT")
            .unwrap_or_else(|_| "0".into())
            .parse()?;

        Ok(Config {
            server: ServerConfig {
                mode,
                bind_addr: std::env::var("WEBCASH_BIND_ADDR")
                    .unwrap_or_else(|_| "0.0.0.0:8080".into()),
                db: DbConfig {
                    backend,
                    redis_url: std::env::var("REDIS_URL").ok(),
                    dynamodb_endpoint: std::env::var("DYNAMODB_ENDPOINT").ok(),
                    fdb_cluster_file: std::env::var("FDB_CLUSTER_FILE").ok(),
                },
                cors_origin: std::env::var("WEBCASH_CORS_ORIGIN").ok(),
                h2: None,
            },
            mining: MiningConfig {
                testnet_difficulty: difficulty,
                initial_difficulty: difficulty,
                reports_per_epoch: 100,
                target_epoch_seconds: 1000,
                initial_mining_amount_wats: mining_amount,
                initial_subsidy_amount_wats: subsidy_amount,
            },
            compute: ComputeConfig {
                backend: compute_backend,
                gpu_batch_threshold: std::env::var("WEBCASH_GPU_THRESHOLD")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1000),
            },
            network: NetworkConfig {
                plane: network_plane,
                dpdk_iface: std::env::var("WEBCASH_DPDK_IFACE").ok(),
                dpdk_dp_path: std::env::var("WEBCASH_DPDK_DP_PATH").ok(),
                dpdk_use_pinned_map: std::env::var("WEBCASH_DPDK_PINNED_MAP")
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false),
            },
        })
    }

    pub fn effective_difficulty(&self) -> u32 {
        match self.server.mode {
            NetworkMode::Testnet => self.mining.testnet_difficulty,
            NetworkMode::Production => self.mining.initial_difficulty,
        }
    }
}
