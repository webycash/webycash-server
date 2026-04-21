pub mod actors;
pub mod api;
pub mod compute;
pub mod config;
pub mod db;
pub mod effects;
pub mod protocol;

use std::sync::Arc;

use actors::batcher::BatchReplacerHandle;
use actors::{LedgerHandle, MinerHandle, StatsHandle, SupervisorHandle};
use compute::ComputeBackend;
use db::LedgerStore;

/// The webcash protocol server.
///
/// Two pluggable backend systems:
/// - **Database** (`LedgerStore`): Redis, DynamoDB, FoundationDB, Redis+FDB
/// - **Compute** (`ComputeBackend`): CPU, CUDA, wgpu
///
/// Constructed via `start()` — fully initialized, immutable after construction.
pub struct WebcashServer<S: LedgerStore> {
    pub server_config: config::ServerConfig,
    pub mining_config: config::MiningConfig,
    #[allow(dead_code)]
    store: Arc<S>,
    supervisor: SupervisorHandle,
    pub compute: Arc<dyn ComputeBackend>,
    pub batcher: BatchReplacerHandle,
}

impl<S: LedgerStore> WebcashServer<S> {
    /// Construct and start the server with supervised actors and compute backend.
    pub async fn start(
        store: S,
        server_config: config::ServerConfig,
        mining_config: config::MiningConfig,
    ) -> anyhow::Result<Self> {
        let store = Arc::new(store);
        let supervisor =
            actors::start_actors(store.clone(), &server_config, &mining_config).await?;
        let compute: Arc<dyn ComputeBackend> = Arc::from(compute::create_backend());
        // Batch coalescing for replace operations — 10K buffer for high concurrency
        let batcher = actors::batcher::spawn_batcher(store.clone(), 10_000);
        tracing::info!(compute = compute.name(), "server initialized");
        Ok(Self {
            server_config,
            mining_config,
            store,
            supervisor,
            compute,
            batcher,
        })
    }

    pub fn ledger(&self) -> LedgerHandle {
        self.supervisor.ledger()
    }

    pub fn miner(&self) -> MinerHandle {
        self.supervisor.miner()
    }

    pub fn stats(&self) -> StatsHandle {
        self.supervisor.stats()
    }

    pub fn compute(&self) -> &Arc<dyn ComputeBackend> {
        &self.compute
    }
}
