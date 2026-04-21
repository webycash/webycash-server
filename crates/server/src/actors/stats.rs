use std::sync::Arc;

use crate::db::{EconomyStats, LedgerStore};
use webycash_macros::gen_server;

pub struct StatsActor {
    store: Arc<dyn LedgerStore>,
}

impl StatsActor {
    pub fn new(store: Arc<dyn LedgerStore>) -> Self {
        Self { store }
    }
}

#[gen_server]
impl StatsActor {
    async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        self.store.get_stats().await
    }
}
