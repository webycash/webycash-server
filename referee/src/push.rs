//! External push-webhook caller.
//!
//! The referee never runs a push-notification service — it calls a
//! configured webhook URL (operated by the deployer's chosen push
//! provider: Web Push, FCM, APNs, custom). Every webhook call follows the
//! shape documented in `docs/push-notification.md` and is authenticated
//! via HMAC-SHA256 over the canonical body, header `X-Push-HMAC`.
//!
//! See also `docs/hook-contract.md` for the recipient-side semantics
//! (the wallet implementor in extro-node implements `insert_hook` and
//! `invalidate_hook`).

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::error::{RefereeError, Result};
use crate::state::{PgpFingerprint, SwapId};

type HmacSha256 = Hmac<Sha256>;

/// What action the recipient wallet should take when the push arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PushKind {
    /// Recipient should call webylib's `insert_hook` with the attached
    /// PGP-encrypted payload — typically a webcash secret addressed to
    /// the recipient's PGP pubkey.
    Insert,
    /// Recipient should call webylib's `invalidate_hook` for the
    /// attached `public_hash` — typically Bob being asked to invalidate
    /// his now-leaked webcash secret on the abort path.
    Invalidate,
    /// Settlement payload — the referee's own MuSig2 partial-sig plus
    /// the encrypted-to-Bob blob containing Alice's partial-sig. Bob's
    /// wallet decrypts Alice's sig and aggregates with the referee's.
    ReleaseSettle,
    /// Refund payload — the referee's MuSig2 partial-sig on `TX_refund`
    /// to Alice (cleartext; she's the recipient).
    ReleaseRefund,
}

/// A single push request the referee dispatches to the configured push
/// provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    /// Stable swap id.
    pub swap_id: SwapId,
    /// PGP fingerprint of the recipient — addressing handle the push
    /// provider uses to find the wallet's registered device.
    pub recipient_pgp_fp: PgpFingerprint,
    /// Action the recipient wallet must take on receipt.
    pub kind: PushKind,
    /// Opaque payload, base64-encoded. Schema depends on `kind` — see
    /// `docs/push-notification.md` §3.
    pub payload_b64: String,
    /// Where the push provider should POST the recipient wallet's ack.
    pub callback_url: String,
}

/// Pluggable push transport. Tests use [`MockPush`]; production uses the
/// HTTP implementation in [`HttpPush`].
#[async_trait]
pub trait PushTransport: Send + Sync + 'static {
    /// Dispatch one push. Returns `Ok(())` if the provider acked our
    /// webhook (200/2xx); errors otherwise. The recipient ack from
    /// `callback_url` is delivered out-of-band to the API endpoint
    /// `/v1/swap/{id}/ack`, NOT through this trait's return value.
    async fn dispatch(&self, req: &PushRequest) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP transport (production)
// ─────────────────────────────────────────────────────────────────────────────

/// Real HTTP push transport. Posts the canonical body to the configured
/// webhook URL with `X-Push-HMAC` header.
pub struct HttpPush {
    /// Webhook URL.
    pub url: String,
    /// 32-byte HMAC key for webhook authentication.
    pub hmac_key: Vec<u8>,
    /// Reqwest client (kept for connection reuse).
    pub client: reqwest::Client,
}

impl HttpPush {
    /// Build with a URL + raw HMAC key bytes.
    pub fn new(url: impl Into<String>, hmac_key: Vec<u8>) -> Self {
        Self {
            url: url.into(),
            hmac_key,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Compute the canonical HMAC over a serialised body.
    ///
    /// Uses the audited `hmac` crate from RustCrypto. The output is
    /// 32 bytes hex-encoded (64 chars) — matches `docs/push-notification.md`.
    pub fn hmac(&self, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.hmac_key)
            .expect("HMAC accepts any key length");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }
}

#[async_trait]
impl PushTransport for HttpPush {
    async fn dispatch(&self, req: &PushRequest) -> Result<()> {
        let body = serde_json::to_vec(req).map_err(RefereeError::from)?;
        let hmac = self.hmac(&body);
        let resp = self
            .client
            .post(&self.url)
            .header("X-Push-HMAC", hmac)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| RefereeError::Push(format!("send: {e}")))?;
        if !resp.status().is_success() {
            return Err(RefereeError::Push(format!(
                "push provider returned {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock transport (tests)
// ─────────────────────────────────────────────────────────────────────────────

/// Mock transport that records every dispatched push for inspection.
#[derive(Default)]
pub struct MockPush {
    /// All pushes seen so far.
    pub seen: std::sync::Mutex<Vec<PushRequest>>,
    /// If set, the next dispatch returns this error and the call is
    /// still recorded.
    pub fail_next: std::sync::Mutex<Option<String>>,
}

impl MockPush {
    /// Build a fresh mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all pushes seen so far.
    pub fn snapshot(&self) -> Vec<PushRequest> {
        self.seen.lock().expect("seen lock").clone()
    }

    /// Schedule the next call to fail.
    pub fn fail_once(&self, msg: &str) {
        *self.fail_next.lock().expect("fail_next lock") = Some(msg.to_string());
    }
}

#[async_trait]
impl PushTransport for MockPush {
    async fn dispatch(&self, req: &PushRequest) -> Result<()> {
        self.seen.lock().expect("seen lock").push(req.clone());
        if let Some(msg) = self.fail_next.lock().expect("fail_next lock").take() {
            return Err(RefereeError::Push(msg));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> PushRequest {
        PushRequest {
            swap_id: SwapId::fresh(),
            recipient_pgp_fp: PgpFingerprint("aa".repeat(20)),
            kind: PushKind::Insert,
            payload_b64: "QUJD".into(),
            callback_url: "https://referee.example/v1/swap/x/ack".into(),
        }
    }

    #[tokio::test]
    async fn mock_records_dispatched() {
        let p = MockPush::new();
        p.dispatch(&req()).await.unwrap();
        p.dispatch(&req()).await.unwrap();
        assert_eq!(p.snapshot().len(), 2);
    }

    #[tokio::test]
    async fn mock_fail_once_then_recovers() {
        let p = MockPush::new();
        p.fail_once("simulated outage");
        let err = p.dispatch(&req()).await.unwrap_err();
        assert!(matches!(err, RefereeError::Push(_)));
        // Next call recovers.
        p.dispatch(&req()).await.unwrap();
        assert_eq!(p.snapshot().len(), 2);
    }

    #[test]
    fn http_hmac_is_deterministic() {
        let a = HttpPush::new("http://x", vec![1, 2, 3, 4]);
        let h1 = a.hmac(b"hello");
        let h2 = a.hmac(b"hello");
        assert_eq!(h1, h2);
        let h3 = a.hmac(b"world");
        assert_ne!(h1, h3);
    }
}
