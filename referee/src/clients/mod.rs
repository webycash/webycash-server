//! HTTP clients for asset rails the referee talks to.
//!
//! The referee makes two kinds of outbound HTTP calls:
//!
//! - `webcash_client` — calls webcash.org's `/api/v1/health_check` for
//!   pre-check + post-check. **Never** calls `/replace` (custodianship);
//!   Alice's wallet is the sole submitter on the webcash leg.
//! - `rgb_client` — calls our RGB server to mint and update the swap-
//!   tracking RGB21 record (the public commitment that this swap exists
//!   and who its parties are).
//!
//! ARK ASP integration is intentionally not here: in the current scope
//! ARK calls happen on the wallet side (Alice constructs the vtxo, signs,
//! broadcasts). When real ARK ASP integration is in scope, an
//! `ark_client` will land alongside the others.
//!
//! Each client is a trait so tests can mock them; production wires up
//! `reqwest`-backed implementations.

use async_trait::async_trait;

use crate::error::Result;
use crate::state::WebcashPublicHash;

/// Health-check status returned by the webcash server for a single hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpentStatus {
    /// Hash exists in the ledger and is unspent.
    Unspent,
    /// Hash exists and is spent.
    Spent,
    /// Hash is unknown (never minted).
    Unknown,
}

/// Pluggable client for webcash-style health-check.
#[async_trait]
pub trait WebcashClient: Send + Sync + 'static {
    /// Look up `hash` on webcash.org's `/api/v1/health_check`.
    async fn check(&self, hash: &WebcashPublicHash) -> Result<SpentStatus>;
}

/// Pluggable client for the RGB server.
#[async_trait]
pub trait RgbClient: Send + Sync + 'static {
    /// Mint the swap-tracking RGB21 record. Returns the record's
    /// stable id (contract id of the record on our RGB server).
    async fn mint_swap_record(&self, swap_id: &str, payload: &serde_json::Value) -> Result<String>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Mocks
// ─────────────────────────────────────────────────────────────────────────────

/// Mock webcash client. Scripted with a sequence of statuses returned
/// in FIFO order; falls back to a default if the script is exhausted.
pub struct MockWebcash {
    /// Default status for an unscripted call.
    pub default_status: SpentStatus,
    /// FIFO scripted overrides.
    pub script: tokio::sync::Mutex<std::collections::VecDeque<SpentStatus>>,
}

impl MockWebcash {
    /// Build with a script + a default fallback.
    pub fn scripted(default: SpentStatus, script: Vec<SpentStatus>) -> Self {
        Self {
            default_status: default,
            script: tokio::sync::Mutex::new(script.into()),
        }
    }

    /// Always-`Unspent` mock.
    pub fn always_unspent() -> Self {
        Self::scripted(SpentStatus::Unspent, vec![])
    }
}

#[async_trait]
impl WebcashClient for MockWebcash {
    async fn check(&self, _hash: &WebcashPublicHash) -> Result<SpentStatus> {
        let next = self.script.lock().await.pop_front();
        Ok(next.unwrap_or(self.default_status))
    }
}

/// Mock RGB client. Records every call and returns deterministic ids.
pub struct MockRgb {
    /// Calls seen.
    pub calls: tokio::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl MockRgb {
    /// Build a fresh mock.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for MockRgb {
    fn default() -> Self {
        Self {
            calls: Default::default(),
        }
    }
}

#[async_trait]
impl RgbClient for MockRgb {
    async fn mint_swap_record(&self, swap_id: &str, payload: &serde_json::Value) -> Result<String> {
        self.calls
            .lock()
            .await
            .push((swap_id.into(), payload.clone()));
        Ok(format!("rgb-record-{swap_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn webcash_mock_consumes_script_then_defaults() {
        let m = MockWebcash::scripted(
            SpentStatus::Unknown,
            vec![SpentStatus::Unspent, SpentStatus::Spent],
        );
        let h = WebcashPublicHash::new("h".repeat(64));
        assert_eq!(m.check(&h).await.unwrap(), SpentStatus::Unspent);
        assert_eq!(m.check(&h).await.unwrap(), SpentStatus::Spent);
        assert_eq!(m.check(&h).await.unwrap(), SpentStatus::Unknown);
    }

    #[tokio::test]
    async fn rgb_mock_records_calls() {
        let m = MockRgb::new();
        let id = m
            .mint_swap_record("abc", &serde_json::json!({"k": 1}))
            .await
            .unwrap();
        assert_eq!(id, "rgb-record-abc");
        assert_eq!(m.calls.lock().await.len(), 1);
    }
}
