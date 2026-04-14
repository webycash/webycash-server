pub mod router;
pub mod service;
pub mod target;
pub mod mining_report;
pub mod replace;
pub mod health_check;
pub mod burn;
pub mod stats;
pub mod terms;

use crate::config::Config;
use crate::db::LedgerStore;
use crate::WebcashServer;

/// Shared application state passed to all handlers.
pub struct AppState<S: LedgerStore = Box<dyn LedgerStore>> {
    pub server: WebcashServer<S>,
    pub config: Config,
}
