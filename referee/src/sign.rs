//! Ed25519 referee identity + canonical-message signing.
//!
//! The referee has a single long-lived Ed25519 keypair. Its public key is
//! served from `GET /v1/pubkey` so wallets can pin it. Every signed
//! message follows the canonical form:
//!
//! ```text
//! "referee:v1:" + tag + ":" + sha256_hex(body_bytes)
//! ```
//!
//! Where `tag` is one of: `initiate-ack`, `zkps-verified`, `pre-checked`,
//! `insert-pushed`, `post-checked`, `settled`, `aborted`, `invalidated`,
//! `refunded`, `audit-tip`. The signed envelope goes into the audit log
//! and into webhook payloads.
//!
//! Reusing the same canonical structure across all messages prevents
//! cross-protocol confusion: a signature produced for an `audit-tip`
//! cannot be replayed as a `settled` verdict because the tag bytes are
//! part of the signed material.

use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};
use sha2::{Digest, Sha256};

use crate::error::{RefereeError, Result};

/// All canonical-message tags the referee uses. Adding a new variant is
/// a deliberate protocol change — every existing signature was made over
/// one of these specific tags and remains tied to it forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    /// Acknowledges the `/v1/swap/initiate` request was accepted.
    InitiateAck,
    /// Both parties' Groth16 ZKPs have been verified.
    ZkpsVerified,
    /// Pre-check: webcash.org confirmed `H_B` unspent.
    PreChecked,
    /// `insert_hook` push was successfully dispatched.
    InsertPushed,
    /// Post-check: webcash.org confirmed `H_B` now spent.
    PostChecked,
    /// Final: settlement is authorised; release-settle payload sent to Bob.
    Settled,
    /// Final: swap aborted; abort path engaged.
    Aborted,
    /// Bob's wallet acked the `invalidate_hook` push.
    Invalidated,
    /// Final (abort path): refund-sig sent to Alice.
    Refunded,
    /// Periodic audit-log tip signature.
    AuditTip,
}

impl Tag {
    /// Stable string used in the canonical message — never change.
    pub const fn as_str(self) -> &'static str {
        match self {
            Tag::InitiateAck => "initiate-ack",
            Tag::ZkpsVerified => "zkps-verified",
            Tag::PreChecked => "pre-checked",
            Tag::InsertPushed => "insert-pushed",
            Tag::PostChecked => "post-checked",
            Tag::Settled => "settled",
            Tag::Aborted => "aborted",
            Tag::Invalidated => "invalidated",
            Tag::Refunded => "refunded",
            Tag::AuditTip => "audit-tip",
        }
    }
}

/// The referee's identity. Holds the Ed25519 signing key in memory and
/// signs canonical messages on request. Single source of authority for
/// "this is the referee speaking".
#[derive(Clone)]
pub struct Identity {
    sk: SigningKey,
    vk: VerifyingKey,
}

impl Identity {
    /// Load from a file containing the 32-byte raw secret hex-encoded.
    /// The file must be 0600.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| RefereeError::Crypto(format!("identity file: {e}")))?;
        let bytes = hex::decode(s.trim())
            .map_err(|e| RefereeError::Crypto(format!("identity hex: {e}")))?;
        if bytes.len() != SECRET_KEY_LENGTH {
            return Err(RefereeError::Crypto(format!(
                "identity must be {SECRET_KEY_LENGTH} bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; SECRET_KEY_LENGTH];
        arr.copy_from_slice(&bytes);
        let sk = SigningKey::from_bytes(&arr);
        let vk = sk.verifying_key();
        Ok(Self { sk, vk })
    }

    /// Build from already-loaded raw secret bytes (for tests).
    pub fn from_secret_bytes(bytes: [u8; SECRET_KEY_LENGTH]) -> Self {
        let sk = SigningKey::from_bytes(&bytes);
        let vk = sk.verifying_key();
        Self { sk, vk }
    }

    /// Public key — served from `GET /v1/pubkey`.
    pub fn pubkey(&self) -> [u8; 32] {
        *self.vk.as_bytes()
    }

    /// Hex of the public key.
    pub fn pubkey_hex(&self) -> String {
        hex::encode(self.pubkey())
    }

    /// Sign a canonical message. The signed payload is
    /// `"referee:v1:" + tag + ":" + sha256_hex(body)`. Returns the
    /// 64-byte detached signature, hex-encoded.
    pub fn sign(&self, tag: Tag, body: &[u8]) -> String {
        let hash = Sha256::digest(body);
        let canonical = format!("referee:v1:{}:{}", tag.as_str(), hex::encode(hash));
        let sig: Signature = self.sk.sign(canonical.as_bytes());
        hex::encode(sig.to_bytes())
    }

    /// Verify a canonical signature. Used by the wallet implementor and
    /// auditors to check the audit log; the referee itself rarely calls
    /// this (it knows what it signed).
    pub fn verify(pubkey: [u8; 32], tag: Tag, body: &[u8], sig_hex: &str) -> Result<()> {
        let vk = VerifyingKey::from_bytes(&pubkey)
            .map_err(|e| RefereeError::Crypto(format!("pubkey: {e}")))?;
        let sig_bytes = hex::decode(sig_hex)
            .map_err(|e| RefereeError::Crypto(format!("sig hex: {e}")))?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| RefereeError::Crypto("sig must be 64 bytes".into()))?;
        let sig = Signature::from_bytes(&sig_arr);
        let hash = Sha256::digest(body);
        let canonical = format!("referee:v1:{}:{}", tag.as_str(), hex::encode(hash));
        vk.verify(canonical.as_bytes(), &sig)
            .map_err(|e| RefereeError::Crypto(format!("sig verify: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let id = Identity::from_secret_bytes([7u8; 32]);
        let body = b"swap=42; outcome=settled";
        let sig = id.sign(Tag::Settled, body);
        Identity::verify(id.pubkey(), Tag::Settled, body, &sig)
            .expect("valid sig must verify");
    }

    #[test]
    fn signature_is_tag_specific() {
        let id = Identity::from_secret_bytes([7u8; 32]);
        let body = b"shared body";
        let sig_settled = id.sign(Tag::Settled, body);
        // Same body, different tag — signature must NOT verify under the wrong tag.
        let err =
            Identity::verify(id.pubkey(), Tag::Aborted, body, &sig_settled).unwrap_err();
        assert!(matches!(err, RefereeError::Crypto(_)));
    }

    #[test]
    fn signature_is_body_specific() {
        let id = Identity::from_secret_bytes([7u8; 32]);
        let sig = id.sign(Tag::Settled, b"body-A");
        let err =
            Identity::verify(id.pubkey(), Tag::Settled, b"body-B", &sig).unwrap_err();
        assert!(matches!(err, RefereeError::Crypto(_)));
    }

    #[test]
    fn pubkey_is_deterministic_from_secret() {
        let a = Identity::from_secret_bytes([42u8; 32]);
        let b = Identity::from_secret_bytes([42u8; 32]);
        assert_eq!(a.pubkey(), b.pubkey());
    }
}
