pub mod redis;
pub mod dynamodb;
#[cfg(feature = "fdb")]
pub mod foundationdb;
#[cfg(feature = "fdb")]
pub mod redis_fdb;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, DbBackend};
use crate::protocol::mining::MiningState;

/// A stored webcash token record on the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub public_hash: String,
    pub amount_wats: i64,
    pub spent: bool,
    pub created_at: DateTime<Utc>,
    pub spent_at: Option<DateTime<Utc>>,
    pub origin: TokenOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenOrigin {
    Mined,
    Replaced,
}

/// Audit record for a replacement (transfer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplacementRecord {
    pub id: String,
    pub input_hashes: Vec<String>,
    pub output_hashes: Vec<String>,
    pub total_amount_wats: i64,
    pub created_at: DateTime<Utc>,
}

/// Audit record for a burn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnRecord {
    pub id: String,
    pub public_hash: String,
    pub amount_wats: i64,
    pub burned_at: DateTime<Utc>,
}

/// Economy statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EconomyStats {
    pub total_circulation_wats: i64,
    pub mining_reports_count: u64,
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub mining_amount_wats: i64,
    pub subsidy_amount_wats: i64,
}

/// The central database abstraction. Every backend implements this single trait.
/// All methods take &self — implementations hold their own connection pools.
#[async_trait]
pub trait LedgerStore: Send + Sync + 'static {
    /// Insert a new unspent token. Fails if hash already exists.
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()>;

    /// Look up a token by its public hash. Returns None if not found.
    async fn get_token(&self, public_hash: &str) -> anyhow::Result<Option<TokenRecord>>;

    /// Mark a token as spent. Returns false if already spent or not found.
    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool>;

    /// Atomic replacement: mark all inputs spent, insert all outputs,
    /// and write the audit record. Entire operation succeeds or fails.
    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()>;

    /// Get current mining state.
    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>>;

    /// Update mining state.
    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()>;

    /// Burn a token: mark spent and write burn audit record.
    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()>;

    /// Check multiple tokens' spent status.
    /// Returns (hash, Option<bool>): None = not found, Some(true) = spent, Some(false) = unspent.
    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>>;

    /// Get economy statistics.
    async fn get_stats(&self) -> anyhow::Result<EconomyStats>;
}

/// Blanket impl so Box<dyn LedgerStore> satisfies LedgerStore.
#[async_trait]
impl LedgerStore for Box<dyn LedgerStore> {
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()> {
        (**self).insert_token(record).await
    }
    async fn get_token(&self, public_hash: &str) -> anyhow::Result<Option<TokenRecord>> {
        (**self).get_token(public_hash).await
    }
    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool> {
        (**self).mark_spent(public_hash).await
    }
    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()> {
        (**self).atomic_replace(inputs, outputs, record).await
    }
    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        (**self).get_mining_state().await
    }
    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        (**self).update_mining_state(state).await
    }
    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        (**self).burn_token(public_hash, record).await
    }
    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        (**self).check_tokens(hashes).await
    }
    async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        (**self).get_stats().await
    }
}

/// Create a LedgerStore from config.
pub async fn create_store(config: &Config) -> anyhow::Result<Box<dyn LedgerStore>> {
    match config.server.db.backend {
        DbBackend::Redis => {
            let url = config
                .server
                .db
                .redis_url
                .as_deref()
                .unwrap_or("redis://127.0.0.1:6379");
            let store = redis::RedisStore::new(url).await?;
            Ok(Box::new(store))
        }
        DbBackend::DynamoDb => {
            let store = dynamodb::DynamoDbStore::new(&config.server.db).await?;
            store.ensure_tables().await?;
            Ok(Box::new(store))
        }
        DbBackend::FoundationDb => {
            #[cfg(feature = "fdb")]
            {
                let cluster_file = config.server.db.fdb_cluster_file.as_deref();
                let store = foundationdb::FdbStore::new(cluster_file)?;
                Ok(Box::new(store))
            }
            #[cfg(not(feature = "fdb"))]
            {
                anyhow::bail!(
                    "FoundationDB backend requires the 'fdb' feature. \
                     Rebuild with: cargo build --features fdb"
                )
            }
        }
        DbBackend::RedisFdb => {
            #[cfg(feature = "fdb")]
            {
                let redis_url = config
                    .server
                    .db
                    .redis_url
                    .as_deref()
                    .unwrap_or("redis://127.0.0.1:6379");
                let cluster_file = config.server.db.fdb_cluster_file.as_deref();
                let store = redis_fdb::RedisFdbStore::new(redis_url, cluster_file).await?;
                Ok(Box::new(store))
            }
            #[cfg(not(feature = "fdb"))]
            {
                anyhow::bail!(
                    "Redis+FDB backend requires the 'fdb' feature. \
                     Rebuild with: cargo build --features fdb"
                )
            }
        }
    }
}
