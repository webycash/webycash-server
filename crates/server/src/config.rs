use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub mining: MiningConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub mode: NetworkMode,
    pub bind_addr: String,
    pub db: DbConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Testnet,
    Production,
}

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

#[derive(Debug, Clone, Deserialize)]
pub struct MiningConfig {
    pub testnet_difficulty: u32,
    pub initial_difficulty: u32,
    pub reports_per_epoch: u64,
    pub target_epoch_seconds: u64,
    pub initial_mining_amount_wats: i64,
    pub initial_subsidy_amount_wats: i64,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load from environment variables (for Lambda deployment).
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
            },
            mining: MiningConfig {
                testnet_difficulty: difficulty,
                initial_difficulty: difficulty,
                reports_per_epoch: 100,
                target_epoch_seconds: 1000,
                initial_mining_amount_wats: mining_amount,
                initial_subsidy_amount_wats: subsidy_amount,
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
