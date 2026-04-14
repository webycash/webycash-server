pub mod actors;
pub mod api;
pub mod config;
pub mod db;
pub mod effects;
pub mod protocol;

use std::sync::Arc;

use actors::{LedgerHandle, MinerHandle, StatsHandle, SupervisorHandle};
use db::LedgerStore;

/// The webcash protocol server. Generic over the database backend.
/// Not a singleton -- constructed in main and passed via Arc.
pub struct WebcashServer<S: LedgerStore> {
    pub server_config: config::ServerConfig,
    pub mining_config: config::MiningConfig,
    store: Arc<S>,
    supervisor: Option<SupervisorHandle>,
}

impl<S: LedgerStore> WebcashServer<S> {
    pub fn new(
        store: S,
        server_config: config::ServerConfig,
        mining_config: config::MiningConfig,
    ) -> Self {
        Self {
            server_config,
            mining_config,
            store: Arc::new(store),
            supervisor: None,
        }
    }

    /// Start supervised actor hierarchy. Must be called before handling requests.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let supervisor =
            actors::start_actors(self.store.clone(), &self.server_config, &self.mining_config)
                .await?;
        self.supervisor = Some(supervisor);
        Ok(())
    }

    fn supervisor(&self) -> &SupervisorHandle {
        self.supervisor.as_ref().expect("server not started")
    }

    pub fn ledger(&self) -> LedgerHandle {
        self.supervisor().ledger()
    }

    pub fn miner(&self) -> MinerHandle {
        self.supervisor().miner()
    }

    pub fn stats(&self) -> StatsHandle {
        self.supervisor().stats()
    }
}
