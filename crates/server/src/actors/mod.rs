pub mod ledger;
pub mod miner;
pub mod stats;
pub mod supervisor;

use std::sync::Arc;

use crate::config::{MiningConfig, ServerConfig};
use crate::db::LedgerStore;

pub use ledger::LedgerHandle;
pub use miner::MinerHandle;
pub use stats::StatsHandle;
pub use supervisor::{ChildHandles, SupervisorHandle};

/// Start the supervised actor hierarchy and return a supervisor handle.
///
/// The supervisor spawns LedgerActor, MinerActor, and StatsActor as linked
/// children with one-for-one restart. If any child crashes, only that child
/// is restarted.
pub async fn start_actors(
    store: Arc<dyn LedgerStore>,
    config: &ServerConfig,
    mining_config: &MiningConfig,
) -> anyhow::Result<SupervisorHandle> {
    supervisor::SupervisorActor::start(store, config, mining_config).await
}
