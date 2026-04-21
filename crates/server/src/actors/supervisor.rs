use std::sync::Arc;

use crate::config::{MiningConfig, ServerConfig};
use crate::db::LedgerStore;

use super::ledger::{LedgerActor, LedgerHandle, LedgerMsg};
use super::miner::{MinerActor, MinerHandle, MinerMsg};
use super::stats::{StatsActor, StatsHandle, StatsMsg};
use webycash_macros::supervisor;

/// The root supervisor. Each `spawn_*` method declares a child actor.
/// The macro generates: SupervisorMsg, SupervisorState, ChildHandles,
/// SupervisorHandle, Actor impl with one-for-one restart.
pub struct SupervisorActor {
    store: Arc<dyn LedgerStore>,
    server_config: ServerConfig,
    mining_config: MiningConfig,
}

impl SupervisorActor {
    pub async fn start(
        store: Arc<dyn LedgerStore>,
        config: &ServerConfig,
        mining_config: &MiningConfig,
    ) -> anyhow::Result<SupervisorHandle> {
        let sup = SupervisorActor {
            store,
            server_config: config.clone(),
            mining_config: mining_config.clone(),
        };
        sup.start_supervisor().await
    }
}

#[supervisor(strategy = one_for_one)]
impl SupervisorActor {
    fn spawn_ledger(&self) -> (LedgerActor, ()) {
        (LedgerActor::new(self.store.clone()), ())
    }

    fn spawn_miner(&self) -> (MinerActor, Arc<dyn LedgerStore>) {
        (
            MinerActor::new(self.store.clone(), &self.server_config, &self.mining_config),
            self.store.clone(),
        )
    }

    fn spawn_stats(&self) -> (StatsActor, ()) {
        (StatsActor::new(self.store.clone()), ())
    }
}
