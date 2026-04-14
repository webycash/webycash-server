use super::LedgerEffect;
use crate::db::LedgerStore;

/// Interpret a LedgerEffect program against a real database backend.
/// Uses iterative trampolining to avoid stack overflow on deep chains.
pub async fn interpret<A: Send + 'static>(
    store: &dyn LedgerStore,
    mut effect: LedgerEffect<A>,
) -> anyhow::Result<A> {
    loop {
        match effect {
            LedgerEffect::Pure(a) => return Ok(a),
            LedgerEffect::Fail { error } => return Err(anyhow::anyhow!(error)),
            LedgerEffect::GetToken { hash, next } => {
                let result = store.get_token(&hash).await?;
                effect = next(result);
            }
            LedgerEffect::InsertToken { record, next } => {
                store.insert_token(&record).await?;
                effect = next(());
            }
            LedgerEffect::MarkSpent { hash, next } => {
                let result = store.mark_spent(&hash).await?;
                effect = next(result);
            }
            LedgerEffect::GetMiningState { next } => {
                let result = store.get_mining_state().await?;
                effect = next(result);
            }
            LedgerEffect::UpdateMiningState { state, next } => {
                store.update_mining_state(&state).await?;
                effect = next(());
            }
            LedgerEffect::AtomicReplace {
                inputs,
                outputs,
                record,
                next,
            } => {
                store.atomic_replace(&inputs, &outputs, &record).await?;
                effect = next(());
            }
        }
    }
}
