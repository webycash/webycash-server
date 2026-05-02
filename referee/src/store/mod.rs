//! Persistent swap-state store.
//!
//! Two implementations:
//!
//! - [`InMemoryStore`] — default, used for dev + tests. State is lost on
//!   restart.
//! - Postgres-backed (gated behind `postgres` feature) — production. The
//!   real implementation lives behind the feature flag because we don't
//!   want every `cargo build -p referee` to drag in `sqlx`'s build tax
//!   for users who only need the in-memory dev binary.
//!
//! ## What the store holds
//!
//! For each swap: the latest `AnyPhaseSwapState` (canonical JSON) and
//! enough metadata to make `WHERE swap_id = ? ORDER BY ts DESC LIMIT 1`
//! cheap. The audit log lives separately (see [`crate::audit`]).
//!
//! ## What the store does NOT hold
//!
//! - Secret nonce material — that lives in a process-local secret store
//!   (see [`crate::musig2`]). Persisting secret nonces across restarts
//!   would let a compromised attacker resume a stale session; if the
//!   referee restarts mid-swap we abort the in-flight swap and the
//!   refund path engages.
//! - Cleartext PGP secrets — by construction, the referee never sees
//!   them.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{RefereeError, Result};
use crate::state::{AnyPhaseSwapState, SwapId};

/// Persisted swap row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSwap {
    /// Stable id.
    pub id: SwapId,
    /// Latest phase-erased state.
    pub state: AnyPhaseSwapState,
    /// When this row was last updated.
    pub updated_at_unix: u64,
}

/// Pluggable store trait.
#[async_trait]
pub trait SwapStore: Send + Sync + 'static {
    /// Persist a new (or updated) swap state.
    async fn upsert(&self, swap: &PersistedSwap) -> Result<()>;

    /// Read latest state by id, or `Ok(None)` if not found.
    async fn get(&self, id: &SwapId) -> Result<Option<PersistedSwap>>;

    /// Mark a swap as terminal — used by garbage collection. Default
    /// implementation is a no-op (terminals stay; cleanup is a separate
    /// concern).
    async fn mark_terminal(&self, _id: &SwapId) -> Result<()> {
        Ok(())
    }
}

/// In-memory store. Use only in tests + dev.
#[derive(Default)]
pub struct InMemoryStore {
    rows: tokio::sync::RwLock<HashMap<String, PersistedSwap>>,
}

#[async_trait]
impl SwapStore for InMemoryStore {
    async fn upsert(&self, swap: &PersistedSwap) -> Result<()> {
        self.rows
            .write()
            .await
            .insert(swap.id.0.clone(), swap.clone());
        Ok(())
    }

    async fn get(&self, id: &SwapId) -> Result<Option<PersistedSwap>> {
        Ok(self.rows.read().await.get(&id.0).cloned())
    }
}

/// Mock store for tests that wants to script failures.
#[derive(Default)]
pub struct MockStore {
    inner: InMemoryStore,
    /// Set this to make the next `upsert` return `Store("...")`.
    pub fail_next_upsert: tokio::sync::Mutex<Option<String>>,
}

#[async_trait]
impl SwapStore for MockStore {
    async fn upsert(&self, swap: &PersistedSwap) -> Result<()> {
        if let Some(msg) = self.fail_next_upsert.lock().await.take() {
            return Err(RefereeError::Store(msg));
        }
        self.inner.upsert(swap).await
    }

    async fn get(&self, id: &SwapId) -> Result<Option<PersistedSwap>> {
        self.inner.get(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, phase: &str) -> PersistedSwap {
        PersistedSwap {
            id: SwapId(id.into()),
            state: AnyPhaseSwapState {
                phase: phase.into(),
                inner: serde_json::json!({"x": 1}),
            },
            updated_at_unix: 1000,
        }
    }

    #[tokio::test]
    async fn upsert_and_get_in_memory() {
        let s = InMemoryStore::default();
        let r = row("a", "init");
        s.upsert(&r).await.unwrap();
        let got = s.get(&SwapId("a".into())).await.unwrap().unwrap();
        assert_eq!(got.state.phase, "init");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let s = InMemoryStore::default();
        let got = s.get(&SwapId("missing".into())).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn mock_can_fail_upsert() {
        let s = MockStore::default();
        *s.fail_next_upsert.lock().await = Some("disk full".into());
        let err = s.upsert(&row("z", "init")).await.unwrap_err();
        assert!(matches!(err, RefereeError::Store(_)));
        // Subsequent call recovers.
        s.upsert(&row("z", "init")).await.unwrap();
    }
}
