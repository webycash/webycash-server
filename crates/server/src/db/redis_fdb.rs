use async_trait::async_trait;

use super::foundationdb::FdbStore;
use super::redis::RedisStore;
use super::{BurnRecord, EconomyStats, LedgerStore, ReplacementRecord, TokenRecord};
use crate::protocol::mining::MiningState;

/// Composite store: FoundationDB as the source of truth, Redis as a write-through cache.
///
/// Write path: FDB first (authoritative), then best-effort Redis update.
/// Read path: Redis first, on miss fall through to FDB and populate cache.
///
/// This gives low-latency reads from Redis while FDB provides ACID durability.
/// If Redis fails on write, the operation still succeeds (FDB is authoritative).
/// If Redis returns stale data on read, the FDB transaction layer catches
/// double-spends at write time.
pub struct RedisFdbStore {
    fdb: FdbStore,
    redis: RedisStore,
}

impl RedisFdbStore {
    pub async fn new(redis_url: &str, fdb_cluster_file: Option<&str>) -> anyhow::Result<Self> {
        let fdb = FdbStore::new(fdb_cluster_file)?;
        let redis = RedisStore::new(redis_url).await?;
        Ok(Self { fdb, redis })
    }
}

#[async_trait]
impl LedgerStore for RedisFdbStore {
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()> {
        // FDB first (authoritative)
        self.fdb.insert_token(record).await?;

        // Best-effort Redis cache populate
        if let Err(e) = self.redis.insert_token(record).await {
            tracing::warn!(
                hash = %record.public_hash,
                error = %e,
                "failed to populate Redis cache after FDB insert"
            );
        }

        Ok(())
    }

    async fn get_token(&self, public_hash: &str) -> anyhow::Result<Option<TokenRecord>> {
        // Try Redis first
        match self.redis.get_token(public_hash).await {
            Ok(Some(record)) => return Ok(Some(record)),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    hash = %public_hash,
                    error = %e,
                    "Redis cache read failed, falling through to FDB"
                );
            }
        }

        // Cache miss or Redis error: read from FDB
        let record = self.fdb.get_token(public_hash).await?;

        // Populate cache on miss
        if let Some(ref r) = record {
            if let Err(e) = self.redis.insert_token(r).await {
                tracing::warn!(
                    hash = %public_hash,
                    error = %e,
                    "failed to populate Redis cache after FDB read"
                );
            }
        }

        Ok(record)
    }

    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool> {
        // FDB first (authoritative, ACID)
        let result = self.fdb.mark_spent(public_hash).await?;

        if result {
            // Best-effort Redis update
            if let Err(e) = self.redis.mark_spent(public_hash).await {
                tracing::warn!(
                    hash = %public_hash,
                    error = %e,
                    "failed to update Redis cache after FDB mark_spent"
                );
            }
        }

        Ok(result)
    }

    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()> {
        // FDB transaction first (authoritative, ACID)
        self.fdb.atomic_replace(inputs, outputs, record).await?;

        // Best-effort Redis cache update
        if let Err(e) = self.redis.atomic_replace(inputs, outputs, record).await {
            tracing::warn!(
                record_id = %record.id,
                error = %e,
                "failed to update Redis cache after FDB atomic_replace"
            );
        }

        Ok(())
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        // Try Redis first
        match self.redis.get_mining_state().await {
            Ok(Some(state)) => return Ok(Some(state)),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Redis cache read failed for mining state, falling through to FDB"
                );
            }
        }

        // Fall through to FDB
        let state = self.fdb.get_mining_state().await?;

        // Populate cache
        if let Some(ref s) = state {
            if let Err(e) = self.redis.update_mining_state(s).await {
                tracing::warn!(
                    error = %e,
                    "failed to populate Redis cache after FDB mining state read"
                );
            }
        }

        Ok(state)
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        // FDB first (authoritative)
        self.fdb.update_mining_state(state).await?;

        // Best-effort Redis update
        if let Err(e) = self.redis.update_mining_state(state).await {
            tracing::warn!(
                error = %e,
                "failed to update Redis cache after FDB mining state write"
            );
        }

        Ok(())
    }

    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        // FDB first (authoritative, ACID)
        self.fdb.burn_token(public_hash, record).await?;

        // Best-effort Redis update
        if let Err(e) = self.redis.burn_token(public_hash, record).await {
            tracing::warn!(
                hash = %public_hash,
                error = %e,
                "failed to update Redis cache after FDB burn"
            );
        }

        Ok(())
    }

    async fn check_tokens(
        &self,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        // Try Redis first for the batch
        match self.redis.check_tokens(hashes).await {
            Ok(results) => {
                // If all tokens were found in Redis, return directly
                let all_found = results.iter().all(|(_, status)| status.is_some());
                if all_found {
                    return Ok(results);
                }

                // Some misses: collect which hashes need FDB lookup
                let mut final_results = Vec::with_capacity(hashes.len());
                let mut missing: Vec<String> = Vec::new();

                for (hash, status) in &results {
                    if status.is_none() {
                        missing.push(hash.clone());
                    }
                }

                // Fetch missing from FDB
                let fdb_results = self.fdb.check_tokens(&missing).await?;
                let mut fdb_map: std::collections::HashMap<String, Option<bool>> =
                    fdb_results.into_iter().collect();

                // Merge results preserving original order
                for (hash, status) in results {
                    if status.is_some() {
                        final_results.push((hash, status));
                    } else {
                        let fdb_status = fdb_map.remove(&hash).flatten();
                        final_results.push((hash, fdb_status));
                    }
                }

                Ok(final_results)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Redis cache read failed for check_tokens, falling through to FDB"
                );
                self.fdb.check_tokens(hashes).await
            }
        }
    }

    async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        // Stats derived from mining state, use same cache-through pattern
        let state = self.get_mining_state().await?;
        match state {
            Some(s) => Ok(EconomyStats {
                total_circulation_wats: s.total_circulation_wats,
                mining_reports_count: s.mining_reports_count,
                difficulty_target_bits: s.difficulty_target_bits,
                epoch: s.epoch,
                mining_amount_wats: s.mining_amount_wats,
                subsidy_amount_wats: s.subsidy_amount_wats,
            }),
            None => Ok(EconomyStats {
                total_circulation_wats: 0,
                mining_reports_count: 0,
                difficulty_target_bits: 0,
                epoch: 0,
                mining_amount_wats: 0,
                subsidy_amount_wats: 0,
            }),
        }
    }
}
