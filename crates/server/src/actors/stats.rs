use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef};

use crate::db::{EconomyStats, LedgerStore};

/// Handle to communicate with the StatsActor.
#[derive(Clone)]
pub struct StatsHandle {
    actor: ActorRef<StatsMsg>,
}

pub enum StatsMsg {
    GetStats {
        reply: tokio::sync::oneshot::Sender<anyhow::Result<EconomyStats>>,
    },
}

pub struct StatsActor {
    store: Arc<dyn LedgerStore>,
}

impl StatsActor {
    /// Create a new StatsActor instance. Does not start it -- use `Actor::spawn`
    /// or `Actor::spawn_linked` (via the supervisor) to run.
    pub fn new(store: Arc<dyn LedgerStore>) -> Self {
        Self { store }
    }

    pub async fn start(store: Arc<dyn LedgerStore>) -> anyhow::Result<StatsHandle> {
        let (actor_ref, _) = Actor::spawn(
            Some("stats".to_string()),
            Self::new(store),
            (),
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to start stats actor: {}", e))?;
        Ok(StatsHandle { actor: actor_ref })
    }
}

// ractor has blanket impl: impl<T: Any + Send + 'static> Message for T

#[async_trait::async_trait]
impl Actor for StatsActor {
    type Msg = StatsMsg;
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
            StatsMsg::GetStats { reply } => {
                let result = self.store.get_stats().await;
                let _ = reply.send(result);
            }
        }
        Ok(())
    }
}

impl StatsHandle {
    /// Construct a handle from a raw actor ref. Used by the supervisor.
    pub fn from_ref(actor: ActorRef<StatsMsg>) -> Self {
        Self { actor }
    }

    pub async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .cast(StatsMsg::GetStats { reply: tx })
            .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
        rx.await?
    }
}
