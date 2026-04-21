use std::str::FromStr;
use std::sync::Arc;

use crate::db::{BurnRecord, LedgerStore, LedgerStoreExt, TokenRecord};
use crate::protocol::{Amount, PublicWebcash, SecretWebcash};
use webycash_macros::gen_server;

/// LedgerActor: serializes token mutations through the actor message queue.
///
/// For maximum throughput, health_check and replace bypass the actor when
/// the database backend provides its own atomicity guarantees (Redis Lua,
/// DynamoDB transactions, FDB serializable isolation). The actor is used
/// for operations that need in-process serialization (insert_mined).
pub struct LedgerActor {
    store: Arc<dyn LedgerStore>,
}

impl LedgerActor {
    pub fn new(store: Arc<dyn LedgerStore>) -> Self {
        Self { store }
    }

    /// Direct store access for operations that bypass the actor queue.
    /// Safe because the database backend guarantees atomicity.
    pub fn store(&self) -> &Arc<dyn LedgerStore> {
        &self.store
    }
}

/// Actor message handlers — insert_mined needs serialization through the actor
/// to prevent duplicate token insertion from concurrent mining reports.
#[gen_server]
impl LedgerActor {
    async fn health_check(
        &self,
        hashes: Vec<String>,
    ) -> anyhow::Result<Vec<(String, Option<bool>, Option<String>)>> {
        Self::do_health_check(&self.store, hashes).await
    }

    async fn replace(
        &self,
        webcashes: Vec<String>,
        new_webcashes: Vec<String>,
    ) -> anyhow::Result<()> {
        crate::effects::replace::execute_replace(self.store.as_ref(), webcashes, new_webcashes)
            .await
    }

    async fn insert_mined(&self, record: TokenRecord) -> anyhow::Result<()> {
        self.store.insert_token(&record).await
    }

    async fn burn(&self, webcashes: Vec<String>) -> anyhow::Result<()> {
        Self::do_burn(&self.store, webcashes).await
    }
}

/// Pure business logic — stateless, can be called from actor or directly.
impl LedgerActor {
    pub async fn do_health_check(
        store: &Arc<dyn LedgerStore>,
        public_webcash_strings: Vec<String>,
    ) -> anyhow::Result<Vec<(String, Option<bool>, Option<String>)>> {
        // Parse all hashes first (pure, no IO)
        let lookup_hashes: Vec<String> = public_webcash_strings
            .iter()
            .map(|full_str| {
                if full_str.contains(":public:") {
                    PublicWebcash::from_str(full_str)
                        .map(|p| p.hash)
                        .map_err(|e| anyhow::anyhow!("invalid public webcash: {e}"))
                } else {
                    Ok(full_str.clone())
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Single batch lookup — one pipelined round-trip
        let tokens = store.get_tokens(&lookup_hashes).await?;

        // Transform results (pure)
        Ok(public_webcash_strings
            .into_iter()
            .zip(tokens)
            .map(|(full_str, token)| match token {
                None => (full_str, None, None),
                Some(t) => (
                    full_str,
                    Some(t.spent),
                    Some(Amount::from_wats(t.amount_wats).to_string()),
                ),
            })
            .collect())
    }

    pub async fn do_burn(
        store: &Arc<dyn LedgerStore>,
        webcashes: Vec<String>,
    ) -> anyhow::Result<()> {
        // Parse all inputs (pure, no IO)
        let now = chrono::Utc::now();
        let burn_ops: Vec<_> = webcashes
            .iter()
            .map(|wc_str| {
                let secret = SecretWebcash::from_str(wc_str)
                    .map_err(|e| anyhow::anyhow!("invalid webcash: {e}"))?;
                let public = secret.to_public();
                let record = BurnRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    public_hash: public.hash.clone(),
                    amount_wats: secret.amount.wats,
                    burned_at: now,
                };
                Ok((public.hash, record))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Single batch burn — backend pipelines all operations
        store.batch_burn(&burn_ops).await
    }
}
