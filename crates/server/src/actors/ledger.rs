use std::str::FromStr;
use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::db::{BurnRecord, LedgerStore, TokenRecord};
use crate::protocol::{Amount, PublicWebcash, SecretWebcash};

/// Handle to communicate with the LedgerActor.
#[derive(Clone)]
pub struct LedgerHandle {
    actor: ActorRef<LedgerMsg>,
}

pub enum LedgerMsg {
    HealthCheck {
        hashes: Vec<String>,
        reply: tokio::sync::oneshot::Sender<
            anyhow::Result<Vec<(String, Option<bool>, Option<String>)>>,
        >,
    },
    Replace {
        webcashes: Vec<String>,
        new_webcashes: Vec<String>,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    InsertMined {
        record: TokenRecord,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    Burn {
        webcashes: Vec<String>,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
}

pub struct LedgerActor {
    store: Arc<dyn LedgerStore>,
}

impl LedgerActor {
    /// Create a new LedgerActor instance. Does not start it -- use `Actor::spawn`
    /// or `Actor::spawn_linked` (via the supervisor) to run.
    pub fn new(store: Arc<dyn LedgerStore>) -> Self {
        Self { store }
    }

    pub async fn start(store: Arc<dyn LedgerStore>) -> anyhow::Result<LedgerHandle> {
        let (actor_ref, _) = Actor::spawn(Some("ledger".to_string()), Self::new(store), ())
            .await
            .map_err(|e| anyhow::anyhow!("failed to start ledger actor: {}", e))?;
        Ok(LedgerHandle { actor: actor_ref })
    }
}

// ractor has blanket impl: impl<T: Any + Send + 'static> Message for T

#[async_trait::async_trait]
impl Actor for LedgerActor {
    type Msg = LedgerMsg;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            LedgerMsg::HealthCheck { hashes, reply } => {
                let result = self.handle_health_check(hashes).await;
                let _ = reply.send(result);
            }
            LedgerMsg::Replace {
                webcashes,
                new_webcashes,
                reply,
            } => {
                let result = self.handle_replace(webcashes, new_webcashes).await;
                let _ = reply.send(result);
            }
            LedgerMsg::InsertMined { record, reply } => {
                let result = self.store.insert_token(&record).await;
                let _ = reply.send(result);
            }
            LedgerMsg::Burn { webcashes, reply } => {
                let result = self.handle_burn(webcashes).await;
                let _ = reply.send(result);
            }
        }
        Ok(())
    }
}

impl LedgerActor {
    async fn handle_health_check(
        &self,
        public_webcash_strings: Vec<String>,
    ) -> anyhow::Result<Vec<(String, Option<bool>, Option<String>)>> {
        let mut results = Vec::with_capacity(public_webcash_strings.len());
        for full_str in &public_webcash_strings {
            // webylib sends full PublicWebcash strings like "e200.00:public:abc..."
            // We need to extract just the hash part for database lookup.
            let lookup_hash = if full_str.contains(":public:") {
                let public = PublicWebcash::from_str(full_str)
                    .map_err(|e| anyhow::anyhow!("invalid public webcash: {}", e))?;
                public.hash
            } else {
                // Bare hash string (64 hex chars) — use as-is
                full_str.clone()
            };

            let token = self.store.get_token(&lookup_hash).await?;
            match token {
                None => results.push((full_str.clone(), None, None)),
                Some(t) => results.push((
                    full_str.clone(),
                    Some(t.spent),
                    Some(Amount::from_wats(t.amount_wats).to_string()),
                )),
            }
        }
        Ok(results)
    }

    async fn handle_replace(
        &self,
        webcashes: Vec<String>,
        new_webcashes: Vec<String>,
    ) -> anyhow::Result<()> {
        // Build the replace operation as a Free Monad effect program.
        // The effect describes validation (GetToken for each input) followed
        // by the atomic replace. The interpreter runs it against the real DB.
        use crate::effects::interpreter::interpret;
        use crate::effects::replace::build_replace_effect;

        let effect = build_replace_effect(webcashes, new_webcashes);
        interpret(self.store.as_ref(), effect).await
    }

    async fn handle_burn(&self, webcashes: Vec<String>) -> anyhow::Result<()> {
        for wc_str in &webcashes {
            let secret = SecretWebcash::from_str(wc_str)
                .map_err(|e| anyhow::anyhow!("invalid webcash: {}", e))?;
            let public = secret.to_public();
            let record = BurnRecord {
                id: uuid::Uuid::new_v4().to_string(),
                public_hash: public.hash.clone(),
                amount_wats: secret.amount.wats,
                burned_at: chrono::Utc::now(),
            };
            self.store.burn_token(&public.hash, &record).await?;
        }
        Ok(())
    }
}

impl LedgerHandle {
    /// Construct a handle from a raw actor ref. Used by the supervisor.
    pub fn from_ref(actor: ActorRef<LedgerMsg>) -> Self {
        Self { actor }
    }

    pub async fn health_check(
        &self,
        hashes: Vec<String>,
    ) -> anyhow::Result<Vec<(String, Option<bool>, Option<String>)>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(LedgerMsg::HealthCheck { hashes, reply: tx })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }

    pub async fn replace(
        &self,
        webcashes: Vec<String>,
        new_webcashes: Vec<String>,
    ) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(LedgerMsg::Replace {
                webcashes,
                new_webcashes,
                reply: tx,
            })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }

    pub async fn insert_mined(&self, record: TokenRecord) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(LedgerMsg::InsertMined { record, reply: tx })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }

    pub async fn burn(&self, webcashes: Vec<String>) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(LedgerMsg::Burn {
                webcashes,
                reply: tx,
            })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }
}
