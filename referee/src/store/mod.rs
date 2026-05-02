//! Persistent swap-state store.
//!
//! Mirrors the asset-server backend matrix: Redis, DynamoDB,
//! FoundationDB. Each backend is gated behind its own cargo feature
//! and uses a referee-specific table/keyspace so it can coexist with
//! the asset-server data on the same cluster.
//!
//! - [`InMemoryStore`] — default; used for dev + tests. State is lost
//!   on restart.
//! - [`MockStore`] — test scaffold for scripting failures.
//! - `RedisSwapStore` (feature `redis`) — uses keys
//!   `referee:swap:{id}` for persisted transaction rows + auxiliary
//!   sorted-sets `referee:by-bob:{fp}` / `referee:by-alice:{fp}`.
//! - `DynamoDbSwapStore` (feature `dynamodb`) — table
//!   `RefereeSwaps{-suffix}` with `pk = swap_id` and GSIs `byBob` /
//!   `byAlice` on the PGP-fingerprint attributes.
//! - `FdbSwapStore` (feature `fdb`) — subspaces
//!   `referee/swap/{id}` (rows) and
//!   `referee/by-{role}/{fp}/{ts}/{id}` (indexes).
//!
//! The audit log lives separately in [`crate::audit`] and follows
//! the same backend matrix.
//!
//! ## What the store holds
//!
//! For each swap: a [`Transaction`] — explicit user-facing fields
//! plus an opaque `state_blob` for the orchestrator's continuation.
//! See `docs/transaction-model.md`.
//!
//! ## What the store does NOT hold
//!
//! - Secret nonce material — that lives in a process-local secret store
//!   (see [`crate::musig2`]). Persisting secret nonces across restarts
//!   would let a compromised attacker resume a stale session; if the
//!   referee restarts mid-swap we abort the in-flight swap and the
//!   refund path engages.
//! - Plaintext PGP payloads — the referee receives ciphertext only;
//!   payloads are addressed to the counterparty's PGP pubkey.

use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};

use crate::error::{RefereeError, Result};
use crate::state::{PgpFingerprint, SwapId};
use crate::transaction::{PartyRole, Transaction, TransactionSummary};

#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "fdb")]
pub mod fdb;
#[cfg(feature = "redis")]
pub mod redis;

/// Pluggable store trait. All methods are single-roundtrip on the
/// chosen backend so the orchestrator can run as a stateless Lambda
/// invocation: read → transition → write → exit.
#[async_trait]
pub trait SwapStore: Send + Sync + 'static {
    /// Persist a new (or updated) [`Transaction`]. Implementations
    /// MUST update the by-Bob and by-Alice indexes atomically with
    /// the row write so the history endpoint never sees a row whose
    /// indexes lag (the in-memory backend uses a single write lock;
    /// DynamoDB uses transactional writes; FDB uses a single
    /// transaction; Redis uses a Lua script wrapping HSET + ZADD).
    async fn upsert(&self, tx: &Transaction) -> Result<()>;

    /// Read the latest transaction by id, or `Ok(None)` if not found.
    async fn get(&self, id: &SwapId) -> Result<Option<Transaction>>;

    /// List all transactions where the supplied fingerprint
    /// participated, in reverse-chronological order (newest first).
    /// Capped at 1000 results — pagination is out of scope for v0.4.
    async fn list_by_party(&self, fp: &PgpFingerprint) -> Result<Vec<TransactionSummary>>;

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
    rows: tokio::sync::RwLock<InMemState>,
}

#[derive(Default)]
struct InMemState {
    rows: HashMap<String, Transaction>,
    /// Inverse-chronological order: BTreeMap keyed by negative
    /// `created_at_unix` so iteration is newest-first.
    by_bob: HashMap<String, BTreeMap<(i128, String), ()>>,
    by_alice: HashMap<String, BTreeMap<(i128, String), ()>>,
}

#[async_trait]
impl SwapStore for InMemoryStore {
    async fn upsert(&self, tx: &Transaction) -> Result<()> {
        let mut w = self.rows.write().await;
        let prev = w.rows.insert(tx.swap_id.0.clone(), tx.clone());
        if prev.is_none() {
            // First write: insert into both indexes.
            let bob_idx = w.by_bob.entry(tx.bob_pgp_fp.0.clone()).or_default();
            bob_idx.insert((-(tx.created_at_unix as i128), tx.swap_id.0.clone()), ());
            let alice_idx = w.by_alice.entry(tx.alice_pgp_fp.0.clone()).or_default();
            alice_idx.insert((-(tx.created_at_unix as i128), tx.swap_id.0.clone()), ());
        }
        Ok(())
    }

    async fn get(&self, id: &SwapId) -> Result<Option<Transaction>> {
        Ok(self.rows.read().await.rows.get(&id.0).cloned())
    }

    async fn list_by_party(&self, fp: &PgpFingerprint) -> Result<Vec<TransactionSummary>> {
        let r = self.rows.read().await;
        let mut out: Vec<TransactionSummary> = Vec::new();
        // Bob-side hits.
        if let Some(idx) = r.by_bob.get(&fp.0) {
            for ((_, id), _) in idx.iter().take(1000) {
                if let Some(tx) = r.rows.get(id) {
                    let role = if tx.alice_pgp_fp == *fp {
                        PartyRole::Both
                    } else {
                        PartyRole::Bob
                    };
                    out.push(tx.summary(role));
                }
            }
        }
        // Alice-side hits — skip ones already merged as Both.
        if let Some(idx) = r.by_alice.get(&fp.0) {
            for ((_, id), _) in idx.iter().take(1000) {
                if let Some(tx) = r.rows.get(id) {
                    if tx.bob_pgp_fp == *fp {
                        // Already counted on the bob side as Both.
                        continue;
                    }
                    out.push(tx.summary(PartyRole::Alice));
                }
            }
        }
        // Final sort newest-first across the merged set.
        out.sort_by(|a, b| b.created_at_unix.cmp(&a.created_at_unix));
        out.truncate(1000);
        Ok(out)
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
    async fn upsert(&self, tx: &Transaction) -> Result<()> {
        if let Some(msg) = self.fail_next_upsert.lock().await.take() {
            return Err(RefereeError::Store(msg));
        }
        self.inner.upsert(tx).await
    }

    async fn get(&self, id: &SwapId) -> Result<Option<Transaction>> {
        self.inner.get(id).await
    }

    async fn list_by_party(&self, fp: &PgpFingerprint) -> Result<Vec<TransactionSummary>> {
        self.inner.list_by_party(fp).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AnyPhaseSwapState;
    use crate::transaction::TransactionStatus;

    fn tx(id: &str, bob: &str, alice: &str, phase: &str, created_at: u64) -> Transaction {
        Transaction {
            swap_id: SwapId(id.into()),
            status: TransactionStatus::for_phase(phase),
            phase: phase.into(),
            terminal: TransactionStatus::for_phase(phase).is_terminal(),
            bob_pgp_fp: PgpFingerprint(bob.into()),
            alice_pgp_fp: PgpFingerprint(alice.into()),
            webcash_public_hash: crate::state::WebcashPublicHash::new("h".repeat(64)),
            vtxo_outpoint_hash: crate::state::ArkOutpointHash("v".repeat(64)),
            tx_settle_hash: "s".repeat(64),
            tx_refund_hash: "r".repeat(64),
            created_at_unix: created_at,
            updated_at_unix: created_at,
            insert_push_attempts: 0,
            cancel_reason: None,
            canceled_by_pgp_fp: None,
            htlc_refund_contract_id: None,
            state_blob: AnyPhaseSwapState {
                phase: phase.into(),
                inner: serde_json::json!({}),
            },
        }
    }

    #[tokio::test]
    async fn upsert_and_get_in_memory() {
        let s = InMemoryStore::default();
        s.upsert(&tx("a", "bob1", "alice1", "init", 1000))
            .await
            .unwrap();
        let got = s.get(&SwapId("a".into())).await.unwrap().unwrap();
        assert_eq!(got.phase, "init");
    }

    #[tokio::test]
    async fn list_by_party_returns_swaps_for_fingerprint() {
        let s = InMemoryStore::default();
        s.upsert(&tx("a", "bob1", "alice1", "init", 1000))
            .await
            .unwrap();
        s.upsert(&tx("b", "bob2", "alice1", "settled", 1500))
            .await
            .unwrap();
        s.upsert(&tx("c", "bob1", "alice2", "refunded", 2000))
            .await
            .unwrap();
        let bob1 = s
            .list_by_party(&PgpFingerprint("bob1".into()))
            .await
            .unwrap();
        assert_eq!(bob1.len(), 2);
        assert_eq!(bob1[0].swap_id.0, "c"); // newest first
        assert_eq!(bob1[1].swap_id.0, "a");
        let alice1 = s
            .list_by_party(&PgpFingerprint("alice1".into()))
            .await
            .unwrap();
        assert_eq!(alice1.len(), 2);
    }

    #[tokio::test]
    async fn list_by_party_dedupes_self_swaps() {
        let s = InMemoryStore::default();
        s.upsert(&tx("self", "fp", "fp", "init", 1000))
            .await
            .unwrap();
        let hits = s.list_by_party(&PgpFingerprint("fp".into())).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].role, PartyRole::Both));
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
        let err = s
            .upsert(&tx("z", "bob", "alice", "init", 1))
            .await
            .unwrap_err();
        assert!(matches!(err, RefereeError::Store(_)));
        // Subsequent call recovers.
        s.upsert(&tx("z", "bob", "alice", "init", 1)).await.unwrap();
    }
}
