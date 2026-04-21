//! Batch coalescing actor for high-throughput replace operations.
//!
//! Collects concurrent replace requests and executes them as a single
//! batch_replace call. Turns N sequential EVALSHA round-trips into 1 batch.
//!
//! Architecture:
//!   N HTTP handlers → BatchReplacer actor → batch_replace(&[N ops]) → Redis pipeline
//!   Each handler awaits its individual result via oneshot channel.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::db::{LedgerStore, ReplaceOp, ReplaceResult};
use crate::effects::replace::parse_and_validate_replace;

/// A pending replace request waiting for batch execution.
struct PendingReplace {
    op: ReplaceOp,
    reply: oneshot::Sender<anyhow::Result<()>>,
}

/// Handle for submitting replace requests to the batcher.
#[derive(Clone)]
pub struct BatchReplacerHandle {
    tx: mpsc::Sender<PendingReplace>,
}

impl BatchReplacerHandle {
    /// Submit a replace request and await the result.
    /// The batcher coalesces this with other concurrent requests.
    pub async fn replace(
        &self,
        webcashes: Vec<String>,
        new_webcashes: Vec<String>,
    ) -> anyhow::Result<()> {
        // Phase 1: Pure validation (no IO, no batching needed)
        let validated = parse_and_validate_replace(webcashes, new_webcashes)?;

        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PendingReplace {
                op: validated.into_op(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("batcher channel closed"))?;

        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("batcher dropped reply"))?
    }
}

/// Spawn the batch coalescing actor.
/// Returns a handle for submitting requests.
pub fn spawn_batcher(store: Arc<dyn LedgerStore>, buffer_size: usize) -> BatchReplacerHandle {
    let (tx, rx) = mpsc::channel(buffer_size);

    tokio::spawn(batcher_loop(store, rx));

    BatchReplacerHandle { tx }
}

/// The batcher event loop. Drains all available requests from the channel,
/// then executes them as a single batch. Yields between batches to collect
/// more concurrent requests.
async fn batcher_loop(store: Arc<dyn LedgerStore>, mut rx: mpsc::Receiver<PendingReplace>) {
    loop {
        // Wait for at least one request
        let first = match rx.recv().await {
            Some(req) => req,
            None => break, // Channel closed, shutdown
        };

        // Drain all immediately available requests (non-blocking)
        let mut batch = vec![first];
        while let Ok(req) = rx.try_recv() {
            batch.push(req);
        }

        let batch_size = batch.len();

        // Execute the entire batch in one call
        let ops: Vec<ReplaceOp> = batch.iter().map(|p| p.op.clone()).collect();
        let results = store.batch_replace(&ops).await;

        // Distribute results to individual callers
        batch
            .into_iter()
            .zip(results)
            .for_each(|(pending, result)| {
                let reply = match result {
                    ReplaceResult::Ok => Ok(()),
                    ReplaceResult::Failed(e) => Err(anyhow::anyhow!("{e}")),
                };
                let _ = pending.reply.send(reply);
            });

        if batch_size > 1 {
            tracing::debug!(batch_size, "batch replace coalesced");
        }
    }
}
