//! Append-only signed audit log.
//!
//! Every phase transition the referee performs is committed to a
//! Merkle-style chain: each entry contains the prior tip's hash, so
//! tampering with any past entry breaks every later signature.
//!
//! The audit log is served read-only at `/v1/audit/{swap_id}` and via
//! periodic snapshots committed to the swap-tracking RGB21 record on the
//! RGB server (so even if the referee's own log is compromised, the
//! external commitment still proves what we publicly claimed at each
//! phase).
//!
//! ## Entry shape
//!
//! ```json
//! {
//!   "swap_id": "...",
//!   "phase": "pre-checked",
//!   "ts_unix": 1714003200,
//!   "prior_tip": "<hex(sha256 of prior entry's bytes)>",
//!   "phase_payload": { ... phase-specific fields ... },
//!   "signature": "<hex Ed25519 signature over the canonical body>"
//! }
//! ```
//!
//! The signature covers the canonical-message
//! `"referee:v1:" + tag + ":" + sha256_hex(entry_bytes_minus_signature)`,
//! where `tag` matches the phase. See `crate::sign`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::sign::Identity;
use crate::state::{tag_for_phase, SwapId};

#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "fdb")]
pub mod fdb;
#[cfg(feature = "redis")]
pub mod redis;

/// One audit log entry. Append-only: never modified after signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Stable swap id.
    pub swap_id: SwapId,
    /// Phase name (matches `Phase::NAME`).
    pub phase: String,
    /// Server-stamped Unix timestamp.
    pub ts_unix: u64,
    /// Hex of `sha256(prior_entry_canonical_body)`. Empty string for
    /// the first entry of a swap.
    pub prior_tip: String,
    /// Phase-specific payload — JSON, opaque to the audit layer.
    pub phase_payload: serde_json::Value,
    /// Hex Ed25519 signature over canonical body. Set by [`AuditLog::append`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
}

impl AuditEntry {
    /// Canonical bytes the signature is computed over (excludes the
    /// signature field itself).
    pub fn canonical_body(&self) -> Vec<u8> {
        let stripped = AuditEntry {
            swap_id: self.swap_id.clone(),
            phase: self.phase.clone(),
            ts_unix: self.ts_unix,
            prior_tip: self.prior_tip.clone(),
            phase_payload: self.phase_payload.clone(),
            signature: String::new(),
        };
        // Canonical JSON serialisation — `serde_json::to_vec` is stable
        // for our schema (no maps with non-deterministic order; struct
        // field order is the declaration order).
        serde_json::to_vec(&stripped).expect("canonical JSON")
    }

    /// Hex of `sha256(canonical_body)` — used as `prior_tip` in the
    /// next entry.
    pub fn tip_hash(&self) -> String {
        hex::encode(Sha256::digest(self.canonical_body()))
    }
}

/// Append-only audit log abstraction. Implementations: [`InMemoryAuditLog`]
/// and (production) Postgres-backed.
#[async_trait::async_trait]
pub trait AuditLog: Send + Sync + 'static {
    /// Append a new (unsigned) entry, signing it with the referee's
    /// identity. Returns the new tip hash.
    async fn append(&self, identity: &Identity, entry: &mut AuditEntry) -> Result<String>;

    /// Read all entries for a swap, oldest-first.
    async fn entries_for(&self, swap_id: &SwapId) -> Result<Vec<AuditEntry>>;

    /// Current tip hash for a swap, or empty string if no entries yet.
    async fn tip_for(&self, swap_id: &SwapId) -> Result<String>;
}

// ─────────────────────────────────────────────────────────────────────────────
// In-memory implementation (default; tests use this)
// ─────────────────────────────────────────────────────────────────────────────

/// In-memory append-only log. Suitable for dev + tests; production
/// deployments use one of the cfg-gated backends (Redis, DynamoDB,
/// FoundationDB) that survives restarts.
#[derive(Default)]
pub struct InMemoryAuditLog {
    entries: tokio::sync::RwLock<Vec<AuditEntry>>,
}

#[async_trait::async_trait]
impl AuditLog for InMemoryAuditLog {
    async fn append(&self, identity: &Identity, entry: &mut AuditEntry) -> Result<String> {
        let canonical = entry.canonical_body();
        let tag = tag_for_phase(&entry.phase);
        entry.signature = identity.sign(tag, &canonical);
        let tip = entry.tip_hash();
        self.entries.write().await.push(entry.clone());
        Ok(tip)
    }

    async fn entries_for(&self, swap_id: &SwapId) -> Result<Vec<AuditEntry>> {
        Ok(self
            .entries
            .read()
            .await
            .iter()
            .filter(|e| &e.swap_id == swap_id)
            .cloned()
            .collect())
    }

    async fn tip_for(&self, swap_id: &SwapId) -> Result<String> {
        Ok(self
            .entries
            .read()
            .await
            .iter()
            .rev()
            .find(|e| &e.swap_id == swap_id)
            .map(|e| e.tip_hash())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::Tag;

    fn entry(phase: &str, prior_tip: &str) -> AuditEntry {
        AuditEntry {
            swap_id: SwapId("swap-1".into()),
            phase: phase.into(),
            ts_unix: 1000,
            prior_tip: prior_tip.into(),
            phase_payload: serde_json::json!({"info": "x"}),
            signature: String::new(),
        }
    }

    #[tokio::test]
    async fn append_signs_and_chains() {
        let id = Identity::from_secret_bytes([7u8; 32]);
        let log = InMemoryAuditLog::default();
        let mut e1 = entry("init", "");
        let tip1 = log.append(&id, &mut e1).await.unwrap();
        assert_eq!(e1.signature.len(), 128);

        let mut e2 = entry("zkps-verified", &tip1);
        let tip2 = log.append(&id, &mut e2).await.unwrap();
        assert_ne!(tip1, tip2);

        let entries = log.entries_for(&SwapId("swap-1".into())).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].prior_tip, "");
        assert_eq!(entries[1].prior_tip, tip1);
    }

    #[tokio::test]
    async fn signatures_verify_against_pubkey() {
        let id = Identity::from_secret_bytes([7u8; 32]);
        let log = InMemoryAuditLog::default();
        let mut e = entry("settled", &"00".repeat(32));
        log.append(&id, &mut e).await.unwrap();
        Identity::verify(id.pubkey(), Tag::Settled, &e.canonical_body(), &e.signature)
            .expect("signature verifies");
    }

    #[tokio::test]
    async fn tip_for_empty_returns_empty_string() {
        let log = InMemoryAuditLog::default();
        let tip = log.tip_for(&SwapId("nonexistent".into())).await.unwrap();
        assert!(tip.is_empty());
    }
}
