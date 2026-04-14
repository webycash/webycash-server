use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio::sync::watch;

use crate::config::{MiningConfig, ServerConfig};
use crate::db::LedgerStore;

use super::ledger::{LedgerActor, LedgerHandle, LedgerMsg};
use super::miner::{MinerActor, MinerHandle, MinerMsg};
use super::stats::{StatsActor, StatsHandle, StatsMsg};

/// Messages the supervisor accepts.
pub enum SupervisorMsg {
    /// Subscribe to child handle updates (returns a watch::Receiver).
    Subscribe {
        reply: tokio::sync::oneshot::Sender<watch::Receiver<ChildHandles>>,
    },
}

/// Snapshot of child actor handles for the API layer.
#[derive(Clone)]
pub struct ChildHandles {
    pub ledger: LedgerHandle,
    pub miner: MinerHandle,
    pub stats: StatsHandle,
}

/// Arguments passed to the supervisor at startup.
pub struct SupervisorArgs {
    pub store: Arc<dyn LedgerStore>,
    pub server_config: ServerConfig,
    pub mining_config: MiningConfig,
}

/// Supervisor state: holds child refs and restart context.
pub struct SupervisorState {
    store: Arc<dyn LedgerStore>,
    server_config: ServerConfig,
    mining_config: MiningConfig,
    ledger_ref: ActorRef<LedgerMsg>,
    miner_ref: ActorRef<MinerMsg>,
    stats_ref: ActorRef<StatsMsg>,
    /// Notifies handle holders when children are replaced after restart.
    handles_tx: watch::Sender<ChildHandles>,
}

/// The root supervisor actor. Uses one-for-one restart strategy:
/// if a child crashes, only that child is restarted.
///
/// Supervision tree:
///   SupervisorActor (one_for_one)
///   +-- LedgerActor  -- token CRUD, serializes mutations
///   +-- MinerActor   -- mining state, PoW validation, difficulty
///   +-- StatsActor   -- cached economy statistics
pub struct SupervisorActor;

/// Handle used by the API layer to reach the supervisor and its children.
#[derive(Clone)]
pub struct SupervisorHandle {
    handles_rx: watch::Receiver<ChildHandles>,
}

impl SupervisorHandle {
    /// Get current child handles (follows restarts via the watch channel).
    pub fn children(&self) -> ChildHandles {
        self.handles_rx.borrow().clone()
    }

    pub fn ledger(&self) -> LedgerHandle {
        self.children().ledger
    }

    pub fn miner(&self) -> MinerHandle {
        self.children().miner
    }

    pub fn stats(&self) -> StatsHandle {
        self.children().stats
    }
}

impl SupervisorActor {
    pub async fn start(
        store: Arc<dyn LedgerStore>,
        server_config: &ServerConfig,
        mining_config: &MiningConfig,
    ) -> anyhow::Result<SupervisorHandle> {
        let args = SupervisorArgs {
            store,
            server_config: server_config.clone(),
            mining_config: mining_config.clone(),
        };

        let (actor_ref, _) = Actor::spawn(
            Some("supervisor".to_string()),
            Self,
            args,
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to start supervisor: {e}"))?;

        // Subscribe to handle updates from the supervisor.
        let (tx, rx) = tokio::sync::oneshot::channel();
        actor_ref
            .cast(SupervisorMsg::Subscribe { reply: tx })
            .map_err(|e| anyhow::anyhow!("supervisor send failed: {e}"))?;
        let handles_rx = rx.await?;

        Ok(SupervisorHandle { handles_rx })
    }
}

#[async_trait::async_trait]
impl Actor for SupervisorActor {
    type Msg = SupervisorMsg;
    type State = SupervisorState;
    type Arguments = SupervisorArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let supervisor_cell = myself.get_cell();

        // Spawn children linked to this supervisor.
        let (ledger_ref, _) = Actor::spawn_linked(
            Some("ledger".to_string()),
            LedgerActor::new(args.store.clone()),
            (),
            supervisor_cell.clone(),
        )
        .await
        .map_err(|e| ActorProcessingErr::from(format!("ledger spawn failed: {e}")))?;

        let (miner_ref, _) = Actor::spawn_linked(
            Some("miner".to_string()),
            MinerActor::new(args.store.clone(), &args.server_config, &args.mining_config),
            args.store.clone(),
            supervisor_cell.clone(),
        )
        .await
        .map_err(|e| ActorProcessingErr::from(format!("miner spawn failed: {e}")))?;

        let (stats_ref, _) = Actor::spawn_linked(
            Some("stats".to_string()),
            StatsActor::new(args.store.clone()),
            (),
            supervisor_cell,
        )
        .await
        .map_err(|e| ActorProcessingErr::from(format!("stats spawn failed: {e}")))?;

        let handles = ChildHandles {
            ledger: LedgerHandle::from_ref(ledger_ref.clone()),
            miner: MinerHandle::from_ref(miner_ref.clone()),
            stats: StatsHandle::from_ref(stats_ref.clone()),
        };

        let (handles_tx, _) = watch::channel(handles);

        tracing::info!("supervisor started with all children linked");

        Ok(SupervisorState {
            store: args.store,
            server_config: args.server_config,
            mining_config: args.mining_config,
            ledger_ref,
            miner_ref,
            stats_ref,
            handles_tx,
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            SupervisorMsg::Subscribe { reply } => {
                let rx = state.handles_tx.subscribe();
                let _ = reply.send(rx);
            }
        }
        Ok(())
    }

    /// One-for-one restart strategy: only restart the failed child.
    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorStarted(cell) => {
                tracing::info!(
                    actor_id = %cell.get_id(),
                    name = ?cell.get_name(),
                    "child actor started"
                );
            }
            SupervisionEvent::ActorTerminated(cell, _, reason) => {
                let name = cell.get_name().unwrap_or_default();
                tracing::warn!(
                    actor_id = %cell.get_id(),
                    %name,
                    ?reason,
                    "child actor terminated -- restarting"
                );
                self.restart_child(&name, &myself, state).await?;
            }
            SupervisionEvent::ActorFailed(cell, err) => {
                let name = cell.get_name().unwrap_or_default();
                tracing::error!(
                    actor_id = %cell.get_id(),
                    %name,
                    error = %err,
                    "child actor failed -- restarting"
                );
                self.restart_child(&name, &myself, state).await?;
            }
            SupervisionEvent::ProcessGroupChanged(_) => {}
        }
        Ok(())
    }
}

impl SupervisorActor {
    async fn restart_child(
        &self,
        name: &str,
        myself: &ActorRef<SupervisorMsg>,
        state: &mut SupervisorState,
    ) -> Result<(), ActorProcessingErr> {
        let supervisor_cell = myself.get_cell();

        match name {
            "ledger" => {
                let (ref_new, _) = Actor::spawn_linked(
                    Some("ledger".to_string()),
                    LedgerActor::new(state.store.clone()),
                    (),
                    supervisor_cell,
                )
                .await
                .map_err(|e| ActorProcessingErr::from(format!("ledger restart failed: {e}")))?;
                state.ledger_ref = ref_new;
                tracing::info!("ledger actor restarted");
            }
            "miner" => {
                let (ref_new, _) = Actor::spawn_linked(
                    Some("miner".to_string()),
                    MinerActor::new(
                        state.store.clone(),
                        &state.server_config,
                        &state.mining_config,
                    ),
                    state.store.clone(),
                    supervisor_cell,
                )
                .await
                .map_err(|e| ActorProcessingErr::from(format!("miner restart failed: {e}")))?;
                state.miner_ref = ref_new;
                tracing::info!("miner actor restarted");
            }
            "stats" => {
                let (ref_new, _) = Actor::spawn_linked(
                    Some("stats".to_string()),
                    StatsActor::new(state.store.clone()),
                    (),
                    supervisor_cell,
                )
                .await
                .map_err(|e| ActorProcessingErr::from(format!("stats restart failed: {e}")))?;
                state.stats_ref = ref_new;
                tracing::info!("stats actor restarted");
            }
            other => {
                tracing::warn!(name = other, "unknown child actor terminated, not restarting");
                return Ok(());
            }
        }

        // Push updated handles to all watchers.
        let handles = ChildHandles {
            ledger: LedgerHandle::from_ref(state.ledger_ref.clone()),
            miner: MinerHandle::from_ref(state.miner_ref.clone()),
            stats: StatsHandle::from_ref(state.stats_ref.clone()),
        };
        let _ = state.handles_tx.send(handles);

        Ok(())
    }
}
