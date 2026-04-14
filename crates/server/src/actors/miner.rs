use std::str::FromStr;
use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::config::{MiningConfig, NetworkMode, ServerConfig};
use crate::db::{LedgerStore, TokenOrigin, TokenRecord};
use crate::protocol::mining::{
    adjust_difficulty, verify_pow, MiningPreimage, MiningState, TargetInfo,
};
use crate::protocol::{Amount, SecretWebcash};

/// Handle to communicate with the MinerActor.
#[derive(Clone)]
pub struct MinerHandle {
    actor: ActorRef<MinerMsg>,
}

/// Messages the MinerActor accepts.
pub enum MinerMsg {
    /// Get current mining target (difficulty, rewards).
    GetTarget {
        reply: tokio::sync::oneshot::Sender<anyhow::Result<TargetInfo>>,
    },
    /// Submit a proof-of-work mining report.
    SubmitMiningReport {
        preimage_str: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<MiningReportResult>>,
    },
}

#[derive(Debug)]
pub struct MiningReportResult {
    pub difficulty_target: u32,
}

pub struct MinerState {
    mining_state: MiningState,
}

/// Maximum allowed timestamp drift (5 minutes into the future).
const MAX_TIMESTAMP_DRIFT_SECS: u64 = 300;

pub struct MinerActor {
    store: Arc<dyn LedgerStore>,
    mode: NetworkMode,
    mining_config: MiningConfig,
}

impl MinerActor {
    /// Create a new MinerActor instance. Does not start it -- use `Actor::spawn`
    /// or `Actor::spawn_linked` (via the supervisor) to run.
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

    pub async fn start(
        store: Arc<dyn LedgerStore>,
        config: &ServerConfig,
        mining_config: &MiningConfig,
    ) -> anyhow::Result<MinerHandle> {
        let (actor_ref, _) = Actor::spawn(
            Some("miner".to_string()),
            Self::new(store.clone(), config, mining_config),
            store,
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to start miner actor: {}", e))?;

        Ok(MinerHandle { actor: actor_ref })
    }
}

// ractor has blanket impl: impl<T: Any + Send + 'static> Message for T

#[async_trait::async_trait]
impl Actor for MinerActor {
    type Msg = MinerMsg;
    type State = MinerState;
    type Arguments = Arc<dyn LedgerStore>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        store: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
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

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            MinerMsg::GetTarget { reply } => {
                let target = state.mining_state.to_target_info();
                let _ = reply.send(Ok(target));
            }
            MinerMsg::SubmitMiningReport {
                preimage_str,
                reply,
            } => {
                let result = self.handle_mining_report(&preimage_str, state).await;
                let _ = reply.send(result);
            }
        }
        Ok(())
    }
}

impl MinerActor {
    async fn handle_mining_report(
        &self,
        preimage_str: &str,
        state: &mut MinerState,
    ) -> anyhow::Result<MiningReportResult> {
        let difficulty = state.mining_state.difficulty_target_bits;

        // 1. Verify proof-of-work: SHA256(preimage) must have >= difficulty leading zero bits
        if !verify_pow(preimage_str, difficulty) {
            anyhow::bail!(
                "proof-of-work does not meet difficulty target of {} bits",
                difficulty
            );
        }

        // 2. Parse preimage JSON
        let preimage: MiningPreimage = serde_json::from_str(preimage_str)
            .map_err(|e| anyhow::anyhow!("invalid preimage JSON: {}", e))?;

        // 3. Validate difficulty matches server target
        if preimage.difficulty != difficulty {
            anyhow::bail!(
                "preimage difficulty {} does not match server target {}",
                preimage.difficulty,
                difficulty
            );
        }

        // 4. Validate timestamp: not too far in the future, not zero
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

        // 5. Validate exactly one webcash output
        if preimage.webcash.len() != 1 {
            anyhow::bail!(
                "preimage must contain exactly 1 webcash output, got {}",
                preimage.webcash.len()
            );
        }

        // ─── PHASE A: VALIDATE EVERYTHING (no writes) ───────────────────

        // 6. Parse and validate all webcash outputs
        let now = chrono::Utc::now();
        let mut token_records = Vec::new();

        for wc_str in &preimage.webcash {
            let secret = SecretWebcash::from_str(wc_str)
                .map_err(|e| anyhow::anyhow!("invalid webcash in preimage: {}", e))?;
            if secret.amount.wats != state.mining_state.mining_amount_wats {
                anyhow::bail!(
                    "webcash amount {} does not match mining amount {}",
                    secret.amount,
                    Amount::from_wats(state.mining_state.mining_amount_wats)
                );
            }
            let public = secret.to_public();
            token_records.push(TokenRecord {
                public_hash: public.hash,
                amount_wats: secret.amount.wats,
                spent: false,
                created_at: now,
                spent_at: None,
                origin: TokenOrigin::Mined,
            });
        }

        // 7. Parse and validate all subsidy outputs
        for sub_str in &preimage.subsidy {
            let secret = SecretWebcash::from_str(sub_str)
                .map_err(|e| anyhow::anyhow!("invalid subsidy in preimage: {}", e))?;
            if secret.amount.wats != state.mining_state.subsidy_amount_wats {
                anyhow::bail!(
                    "subsidy amount {} does not match expected subsidy {}",
                    secret.amount,
                    Amount::from_wats(state.mining_state.subsidy_amount_wats)
                );
            }
            let public = secret.to_public();
            token_records.push(TokenRecord {
                public_hash: public.hash,
                amount_wats: secret.amount.wats,
                spent: false,
                created_at: now,
                spent_at: None,
                origin: TokenOrigin::Mined,
            });
        }

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
        // If state update succeeds but token insert fails, the token hash
        // is unique — retrying with a different secret will succeed.
        // Circulation is always accurate.

        let mut new_state = state.mining_state.clone();
        new_state.mining_reports_count += 1;
        new_state.total_circulation_wats = new_circulation;
        new_state.aggregate_work += 2f64.powi(difficulty as i32);

        // 9. Difficulty adjustment (production mode only — testnet stays constant)
        if self.mode == NetworkMode::Production
            && new_state.mining_reports_count % self.mining_config.reports_per_epoch == 0
        {
            let elapsed = (chrono::Utc::now() - new_state.last_adjustment_at)
                .num_seconds()
                .unsigned_abs();
            new_state.difficulty_target_bits = adjust_difficulty(
                new_state.difficulty_target_bits,
                elapsed,
                self.mining_config.target_epoch_seconds,
                self.mining_config.reports_per_epoch,
                self.mining_config.reports_per_epoch,
            );
            new_state.epoch += 1;
            new_state.last_adjustment_at = chrono::Utc::now();
        }

        // 10. Persist mining state first (source of truth for circulation)
        self.store.update_mining_state(&new_state).await?;

        // 11. Insert all token records
        for record in &token_records {
            self.store.insert_token(record).await?;
        }

        // Commit in-memory state only after all writes succeed
        state.mining_state = new_state;

        Ok(MiningReportResult {
            difficulty_target: state.mining_state.difficulty_target_bits,
        })
    }
}

impl MinerHandle {
    /// Construct a handle from a raw actor ref. Used by the supervisor.
    pub fn from_ref(actor: ActorRef<MinerMsg>) -> Self {
        Self { actor }
    }

    pub async fn get_target(&self) -> anyhow::Result<TargetInfo> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(MinerMsg::GetTarget { reply: tx })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }

    pub async fn submit_mining_report(
        &self,
        preimage: String,
    ) -> anyhow::Result<MiningReportResult> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(MinerMsg::SubmitMiningReport {
                preimage_str: preimage,
                reply: tx,
            })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }
}
