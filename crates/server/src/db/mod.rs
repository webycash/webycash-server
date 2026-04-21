pub mod dynamodb;
#[cfg(feature = "fdb")]
pub mod foundationdb;
pub mod redis;
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EconomyStats {
    pub total_circulation_wats: i64,
    pub mining_reports_count: u64,
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub mining_amount_wats: i64,
    pub subsidy_amount_wats: i64,
}

/// A single replace operation within a batch.
#[derive(Debug, Clone)]
pub struct ReplaceOp {
    pub inputs: Vec<String>,
    pub outputs: Vec<TokenRecord>,
    pub record: ReplacementRecord,
}

/// Result of a single replace operation within a batch.
#[derive(Debug)]
pub enum ReplaceResult {
    Ok,
    Failed(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// LedgerStore: batch-native trait.
//
// Every operation is batch-first. Single operations are batches of 1.
// Backends implement the batch methods and decide internally how to execute
// them (Redis pipeline, DynamoDB BatchWriteItem, FDB transaction batching).
// ─────────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LedgerStore: Send + Sync + 'static {
    // ── Token operations ─────────────────────────────────────────────

    /// Insert tokens. Each must have a unique hash — duplicates fail.
    /// Backend pipelines all inserts in minimal round-trips.
    async fn insert_tokens(&self, records: &[TokenRecord]) -> anyhow::Result<()>;

    /// Look up tokens by public hash. Returns in same order as input.
    /// Backend pipelines all lookups in minimal round-trips.
    async fn get_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<Option<TokenRecord>>>;

    /// Check spent status for multiple tokens.
    /// Returns (hash, Option<bool>) in same order: None = not found.
    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>>;

    // ── Atomic replace (batch) ───────────────────────────────────────

    /// Execute a batch of atomic replace operations.
    /// Each operation independently succeeds or fails.
    /// Backend pipelines all operations in minimal round-trips.
    /// A batch of 1 is a single replace — this is the primary API.
    async fn batch_replace(&self, ops: &[ReplaceOp]) -> Vec<ReplaceResult>;

    // ── Burn (batch) ─────────────────────────────────────────────────

    /// Burn multiple tokens. Each independently verified and marked spent.
    async fn batch_burn(&self, ops: &[(String, BurnRecord)]) -> anyhow::Result<()>;

    // ── Mining state ─────────────────────────────────────────────────

    /// Get current mining state.
    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>>;

    /// Update mining state.
    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()>;

    // ── Derived ──────────────────────────────────────────────────────

    /// Get economy statistics. Default: derived from mining state.
    async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        Ok(self
            .get_mining_state()
            .await?
            .map(|s| EconomyStats {
                total_circulation_wats: s.total_circulation_wats,
                mining_reports_count: s.mining_reports_count,
                difficulty_target_bits: s.difficulty_target_bits,
                epoch: s.epoch,
                mining_amount_wats: s.mining_amount_wats,
                subsidy_amount_wats: s.subsidy_amount_wats,
            })
            .unwrap_or_default())
    }
}

// ── Convenience: single-operation wrappers ──────────────────────────────────
// These exist so callers that need only one operation don't have to build a
// batch. They delegate to the batch methods.

/// Extension methods for single-operation convenience.
#[async_trait]
pub trait LedgerStoreExt: LedgerStore {
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()> {
        self.insert_tokens(std::slice::from_ref(record)).await
    }

    async fn get_token(&self, hash: &str) -> anyhow::Result<Option<TokenRecord>> {
        Ok(self
            .get_tokens(&[hash.to_string()])
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()> {
        let op = ReplaceOp {
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
            record: record.clone(),
        };
        let results = self.batch_replace(&[op]).await;
        match results.into_iter().next() {
            Some(ReplaceResult::Ok) => Ok(()),
            Some(ReplaceResult::Failed(e)) => Err(anyhow::anyhow!("{e}")),
            None => Err(anyhow::anyhow!("no result from batch_replace")),
        }
    }

    async fn burn_token(&self, hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        self.batch_burn(&[(hash.to_string(), record.clone())]).await
    }

    async fn mark_spent(&self, hash: &str) -> anyhow::Result<bool> {
        let tokens = self.get_tokens(&[hash.to_string()]).await?;
        match tokens.into_iter().next().flatten() {
            None => Ok(false),
            Some(t) if t.spent => Ok(false),
            Some(_) => {
                // Use a dummy burn record to mark spent via the batch interface
                let record = BurnRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    public_hash: hash.to_string(),
                    amount_wats: 0,
                    burned_at: chrono::Utc::now(),
                };
                self.batch_burn(&[(hash.to_string(), record)]).await?;
                Ok(true)
            }
        }
    }
}

// Blanket impl: every LedgerStore gets the convenience methods
impl<T: LedgerStore + ?Sized> LedgerStoreExt for T {}

/// Blanket impl so Box<dyn LedgerStore> satisfies LedgerStore.
#[async_trait]
impl LedgerStore for Box<dyn LedgerStore> {
    async fn insert_tokens(&self, records: &[TokenRecord]) -> anyhow::Result<()> {
        (**self).insert_tokens(records).await
    }
    async fn get_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<Option<TokenRecord>>> {
        (**self).get_tokens(hashes).await
    }
    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        (**self).check_tokens(hashes).await
    }
    async fn batch_replace(&self, ops: &[ReplaceOp]) -> Vec<ReplaceResult> {
        (**self).batch_replace(ops).await
    }
    async fn batch_burn(&self, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        (**self).batch_burn(ops).await
    }
    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        (**self).get_mining_state().await
    }
    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        (**self).update_mining_state(state).await
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
                let network = unsafe { ::foundationdb::boot() };
                std::mem::forget(network);
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
                let network = unsafe { ::foundationdb::boot() };
                std::mem::forget(network);
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
