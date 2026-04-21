use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use ractor::ActorProcessingErr;

use crate::config::{MiningConfig, NetworkMode, ServerConfig};
use crate::db::{LedgerStore, TokenOrigin, TokenRecord};
use crate::protocol::mining::{adjust_difficulty, verify_and_parse, MiningState, TargetInfo};
use crate::protocol::{Amount, SecretWebcash};
use webycash_macros::gen_server;

#[derive(Debug)]
pub struct MiningReportResult {
    pub difficulty_target: u32,
}

pub struct MinerState {
    mining_state: MiningState,
}

/// Maximum allowed timestamp drift (5 minutes into the future).
const MAX_TIMESTAMP_DRIFT_SECS: u64 = 300;
/// Maximum allowed timestamp age (5 minutes in the past).
const MIN_TIMESTAMP_AGE_SECS: u64 = 300;

pub struct MinerActor {
    store: Arc<dyn LedgerStore>,
    mode: NetworkMode,
    mining_config: MiningConfig,
}

impl MinerActor {
    pub fn new(
        store: Arc<dyn LedgerStore>,
        config: &ServerConfig,
        mining_config: &MiningConfig,
    ) -> Self {
        Self {
            store,
            mode: config.mode.clone(),
            mining_config: mining_config.clone(),
        }
    }
}

#[gen_server(state = MinerState, args = Arc<dyn LedgerStore>)]
impl MinerActor {
    async fn pre_start(
        &self,
        store: Arc<dyn LedgerStore>,
    ) -> Result<MinerState, ActorProcessingErr> {
        let mining_state = match store
            .get_mining_state()
            .await
            .map_err(|e| ActorProcessingErr::from(e.to_string()))?
        {
            Some(state) => state,
            None => {
                let initial = MiningState::initial(
                    self.mining_config.testnet_difficulty,
                    self.mining_config.initial_mining_amount_wats,
                    self.mining_config.initial_subsidy_amount_wats,
                );
                store
                    .update_mining_state(&initial)
                    .await
                    .map_err(|e| ActorProcessingErr::from(e.to_string()))?;
                initial
            }
        };
        Ok(MinerState { mining_state })
    }

    async fn get_target(&self, state: &mut MinerState) -> anyhow::Result<TargetInfo> {
        Ok(state.mining_state.to_target_info())
    }

    async fn submit_mining_report(
        &self,
        state: &mut MinerState,
        preimage_str: String,
    ) -> anyhow::Result<MiningReportResult> {
        self.handle_mining_report(&preimage_str, state).await
    }
}

/// Business logic — pure validation + immutable state transitions.
impl MinerActor {
    async fn handle_mining_report(
        &self,
        preimage_str: &str,
        state: &mut MinerState,
    ) -> anyhow::Result<MiningReportResult> {
        let difficulty = state.mining_state.difficulty_target_bits;

        // 1-2. Verify PoW and parse preimage (accepts raw JSON and base64)
        let preimage = verify_and_parse(preimage_str, difficulty)?;

        // 3. Validate difficulty matches server target
        if preimage.difficulty != difficulty {
            anyhow::bail!(
                "preimage difficulty {} does not match server target {}",
                preimage.difficulty,
                difficulty
            );
        }

        // 4. Validate timestamp: not in the future, not in the past
        let now_secs = chrono::Utc::now().timestamp() as u64;
        if preimage.timestamp == 0 {
            anyhow::bail!("preimage timestamp must not be zero");
        }
        if preimage.timestamp > now_secs + MAX_TIMESTAMP_DRIFT_SECS {
            anyhow::bail!(
                "preimage timestamp {} is too far in the future (server time: {})",
                preimage.timestamp,
                now_secs
            );
        }
        if preimage.timestamp < now_secs.saturating_sub(MIN_TIMESTAMP_AGE_SECS) {
            anyhow::bail!(
                "preimage timestamp {} is too far in the past (server time: {})",
                preimage.timestamp,
                now_secs
            );
        }

        // 5. Validate webcash outputs (1 or more, total must equal mining_amount)
        if preimage.webcash.is_empty() {
            anyhow::bail!("preimage must contain at least 1 webcash output");
        }

        // ─── PHASE A: VALIDATE EVERYTHING (no writes) ───────────────────

        let now = chrono::Utc::now();

        // 6. Parse and validate webcash outputs — total must equal mining_amount
        let webcash_records: Vec<TokenRecord> = preimage
            .webcash
            .iter()
            .map(|wc_str| {
                let secret = SecretWebcash::from_str(wc_str)
                    .map_err(|e| anyhow::anyhow!("invalid webcash in preimage: {}", e))?;
                let public = secret.to_public();
                Ok(TokenRecord {
                    public_hash: public.hash,
                    amount_wats: secret.amount.wats,
                    spent: false,
                    created_at: now,
                    spent_at: None,
                    origin: TokenOrigin::Mined,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let webcash_total_wats: i64 = webcash_records.iter().map(|r| r.amount_wats).sum();
        if webcash_total_wats != state.mining_state.mining_amount_wats {
            anyhow::bail!(
                "webcash total {} does not match mining amount {}",
                Amount::from_wats(webcash_total_wats),
                Amount::from_wats(state.mining_state.mining_amount_wats)
            );
        }

        // 7. Parse and validate subsidy outputs
        let subsidy_records: Vec<TokenRecord> = preimage
            .subsidy
            .iter()
            .map(|sub_str| {
                let secret = SecretWebcash::from_str(sub_str)
                    .map_err(|e| anyhow::anyhow!("invalid subsidy in preimage: {}", e))?;
                if state.mining_state.subsidy_amount_wats > 0
                    && secret.amount.wats != state.mining_state.subsidy_amount_wats
                {
                    anyhow::bail!(
                        "subsidy amount {} does not match expected subsidy {}",
                        secret.amount,
                        Amount::from_wats(state.mining_state.subsidy_amount_wats)
                    );
                }
                let public = secret.to_public();
                Ok(TokenRecord {
                    public_hash: public.hash,
                    amount_wats: secret.amount.wats,
                    spent: false,
                    created_at: now,
                    spent_at: None,
                    origin: TokenOrigin::Mined,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Deduplicate by public hash — first occurrence wins.
        // BTreeMap::collect keeps last, so we reverse to keep first, then extract values.
        let token_records: Vec<TokenRecord> = webcash_records
            .into_iter()
            .chain(subsidy_records)
            .rev()
            .map(|r| (r.public_hash.clone(), r))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();

        // 8. Compute new mining state with checked arithmetic (no writes yet)
        let total_mined = state
            .mining_state
            .mining_amount_wats
            .checked_add(
                state
                    .mining_state
                    .subsidy_amount_wats
                    .checked_mul(preimage.subsidy.len() as i64)
                    .ok_or_else(|| anyhow::anyhow!("subsidy multiplication overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("mining amount overflow"))?;

        let new_circulation = state
            .mining_state
            .total_circulation_wats
            .checked_add(total_mined)
            .ok_or_else(|| anyhow::anyhow!("circulation overflow"))?;

        // ─── PHASE B: WRITE ALL (validation passed) ─────────────────────
        // Write mining state FIRST (authoritative), then tokens.

        // 9. Compute new state as immutable transition (no field mutation)
        let next_count = state.mining_state.mining_reports_count + 1;
        let (new_difficulty, new_epoch, new_adj_time) = if self.mode == NetworkMode::Production
            && next_count % self.mining_config.reports_per_epoch == 0
        {
            let elapsed = (chrono::Utc::now() - state.mining_state.last_adjustment_at)
                .num_seconds()
                .unsigned_abs();
            (
                adjust_difficulty(
                    state.mining_state.difficulty_target_bits,
                    elapsed,
                    self.mining_config.target_epoch_seconds,
                    self.mining_config.reports_per_epoch,
                    self.mining_config.reports_per_epoch,
                ),
                state.mining_state.epoch + 1,
                chrono::Utc::now(),
            )
        } else {
            (
                state.mining_state.difficulty_target_bits,
                state.mining_state.epoch,
                state.mining_state.last_adjustment_at,
            )
        };

        let new_state = MiningState {
            mining_reports_count: next_count,
            total_circulation_wats: new_circulation,
            aggregate_work: state.mining_state.aggregate_work + 2f64.powi(difficulty as i32),
            difficulty_target_bits: new_difficulty,
            epoch: new_epoch,
            last_adjustment_at: new_adj_time,
            ..state.mining_state.clone()
        };

        // 10. Persist mining state first (source of truth for circulation)
        self.store.update_mining_state(&new_state).await?;

        // 11. Insert all token records — single pipelined batch
        self.store.insert_tokens(&token_records).await?;

        // Commit in-memory state only after all writes succeed
        state.mining_state = new_state;

        Ok(MiningReportResult {
            difficulty_target: state.mining_state.difficulty_target_bits,
        })
    }
}
