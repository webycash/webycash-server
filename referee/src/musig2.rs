//! MuSig2 nonce + partial-sig handling for the referee.
//!
//! The referee owns one secp256k1 keypair. For each swap it runs **two**
//! independent MuSig2 sessions — one for `TX_settle`, one for
//! `TX_refund` — each with its own fresh nonce pair. Alice runs symmetric
//! sessions on her side; the referee receives Alice's pub-nonce
//! commitments at swap-init and never sees Alice's secret nonces.
//!
//! ## Cryptographic invariants
//!
//! Restated from `docs/musig2-ceremony.md`:
//!
//! 1. **Nonce-pair freshness**: each session uses a fresh secret-nonce
//!    pair. Reuse is fatal (key recovery). Enforced here by generating
//!    nonces from a CSPRNG at session creation; secret nonces are zeroed
//!    on signing.
//! 2. **Asymmetric partials**: Alice's `TX_settle` partial-sig is
//!    submitted to the referee as ciphertext addressed to Bob's PGP
//!    pubkey, so only Bob ever sees plaintext. Her `TX_refund`
//!    partial-sig is never submitted to the referee at all. The referee
//!    only produces its own partial-sigs. Combining happens on Bob's
//!    side (settlement) or Alice's side (refund).
//! 3. **Session-binding**: every partial-sig is bound to a specific
//!    session id; replay across sessions is rejected.
//!
//! ## Pluggable signer
//!
//! Like the ZKP layer, the MuSig2 layer is a trait so tests use a mock
//! signer with deterministic outputs and production deployments use the
//! `musig2` crate behind the `musig2-real` feature flag.

use async_trait::async_trait;

use crate::error::{RefereeError, Result};
use crate::state::SwapId;

/// 33-byte compressed secp256k1 pubkey, hex.
pub use crate::state::Secp256k1Pubkey;

/// Which signing session a partial-sig is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Session {
    /// Settlement transaction — releases vtxo to Bob.
    Settle,
    /// Refund transaction — returns vtxo to Alice.
    Refund,
}

/// A 66-byte compressed MuSig2 pub-nonce, hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubNonce(pub String);

/// A MuSig2 partial-signature (32 bytes scalar in MuSig2/BIP327), hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialSig(pub String);

/// Pluggable MuSig2 signer that owns the referee's secp256k1 keypair.
///
/// All operations are async because real implementations may proxy to a
/// hardware HSM (production) or a remote signer (split-ops deployment).
#[async_trait]
pub trait Musig2Signer: Send + Sync + 'static {
    /// The referee's MuSig2 pub-share (33-byte compressed, hex). Stable
    /// across all swaps — this is the referee's long-lived MuSig2 key.
    fn pubshare(&self) -> Secp256k1Pubkey;

    /// Begin a session for `swap_id` × `session`. Returns the referee's
    /// public nonce; the secret nonce is held internally keyed by
    /// `(swap_id, session)`.
    async fn begin_session(&self, swap_id: &SwapId, session: Session) -> Result<PubNonce>;

    /// Produce the referee's partial-sig for `(swap_id, session)`,
    /// against `tx_hash` (canonical hash of the transaction being
    /// signed) and Alice's published pubshare + nonce. Consumes the
    /// secret nonce internally.
    async fn partial_sign(
        &self,
        swap_id: &SwapId,
        session: Session,
        tx_hash: &[u8],
        alice_pubshare: &Secp256k1Pubkey,
        alice_pub_nonce: &PubNonce,
    ) -> Result<PartialSig>;

    /// Drop the secret nonce for `(swap_id, session)` without producing a
    /// signature. Called on terminal abort paths so we don't leak unused
    /// nonces in memory.
    async fn discard_session(&self, swap_id: &SwapId, session: Session) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock signer
// ─────────────────────────────────────────────────────────────────────────────

/// Mock MuSig2 signer with deterministic outputs. Used in tests.
pub struct MockSigner {
    pubshare: Secp256k1Pubkey,
    sessions: std::sync::Mutex<std::collections::HashSet<(String, Session)>>,
}

impl MockSigner {
    /// Construct with a fixed (mock) pubshare.
    pub fn new() -> Self {
        Self {
            // Mock pubshare: 02 + 32 bytes of 0x42 — formally a valid-shape
            // compressed point, even/odd parity, 33 bytes. Real pubshares
            // come from secp256k1 key generation.
            pubshare: Secp256k1Pubkey(format!("02{}", "42".repeat(32))),
            sessions: Default::default(),
        }
    }
}

impl Default for MockSigner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Musig2Signer for MockSigner {
    fn pubshare(&self) -> Secp256k1Pubkey {
        self.pubshare.clone()
    }

    async fn begin_session(&self, swap_id: &SwapId, session: Session) -> Result<PubNonce> {
        let mut g = self.sessions.lock().expect("sessions lock");
        let key = (swap_id.0.clone(), session);
        if !g.insert(key) {
            return Err(RefereeError::Musig2(format!(
                "session already begun for {swap_id:?} × {session:?}"
            )));
        }
        // Mock pub-nonce: deterministic from swap_id + session, 66 bytes hex.
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(swap_id.0.as_bytes());
        h.update(match session {
            Session::Settle => b"settle",
            Session::Refund => b"refund",
        });
        let half = hex::encode(h.finalize());
        Ok(PubNonce(format!("{half}{half}")))
    }

    async fn partial_sign(
        &self,
        swap_id: &SwapId,
        session: Session,
        tx_hash: &[u8],
        _alice_pubshare: &Secp256k1Pubkey,
        _alice_pub_nonce: &PubNonce,
    ) -> Result<PartialSig> {
        let mut g = self.sessions.lock().expect("sessions lock");
        let key = (swap_id.0.clone(), session);
        if !g.remove(&key) {
            return Err(RefereeError::Musig2(format!(
                "no live session for {swap_id:?} × {session:?}"
            )));
        }
        // Mock partial-sig: hash of (swap_id, session, tx_hash). 32 bytes hex.
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(swap_id.0.as_bytes());
        h.update(match session {
            Session::Settle => b"settle",
            Session::Refund => b"refund",
        });
        h.update(tx_hash);
        Ok(PartialSig(hex::encode(h.finalize())))
    }

    async fn discard_session(&self, swap_id: &SwapId, session: Session) -> Result<()> {
        let mut g = self.sessions.lock().expect("sessions lock");
        g.remove(&(swap_id.0.clone(), session));
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Real `musig2` crate signer (gated)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "musig2-real")]
mod real_signer {
    //! Real MuSig2 signer using the `musig2` crate over secp256k1
    //! (BIP327).
    //!
    //! The referee's secp256k1 secret key is loaded from a 32-byte
    //! file at boot. Per-session secret nonces are generated with a
    //! CSPRNG and held in process memory keyed by
    //! `(swap_id, session)` — they are never written to the store
    //! (a stateless Lambda restart aborts in-flight swaps and the
    //! refund path engages, per `docs/musig2-ceremony.md`).
    //!
    //! Wire-format: nonces are 66-byte serialized `PubNonce`s, hex;
    //! partial signatures are 32-byte scalars, hex. Tests round-trip
    //! through the same hex encoding the wallet implementor uses.
    use super::*;
    use ::musig2::{KeyAggContext, PartialSignature, PubNonce as MusigPubNonce, SecNonce};
    use rand::RngCore;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};
    use std::collections::HashMap;
    use std::path::Path;

    struct LiveSession {
        sec_nonce: SecNonce,
        pub_nonce: MusigPubNonce,
    }

    /// Production MuSig2 signer.
    pub struct RealSigner {
        sk: SecretKey,
        pk: PublicKey,
        sessions: tokio::sync::Mutex<HashMap<(String, Session), LiveSession>>,
    }

    impl RealSigner {
        /// Load from a file containing the 32-byte raw secret,
        /// hex-encoded. The file must be 0600.
        pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
            let raw = std::fs::read_to_string(path.as_ref())
                .map_err(|e| RefereeError::Crypto(format!("musig2 secret file: {e}")))?;
            let bytes = hex::decode(raw.trim())
                .map_err(|e| RefereeError::Crypto(format!("musig2 secret hex: {e}")))?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| RefereeError::Crypto("musig2 secret must be 32 bytes".into()))?;
            let sk = SecretKey::from_slice(&arr)
                .map_err(|e| RefereeError::Crypto(format!("musig2 secret: {e}")))?;
            let secp = Secp256k1::new();
            let pk = sk.public_key(&secp);
            Ok(Self {
                sk,
                pk,
                sessions: Default::default(),
            })
        }
    }

    fn parse_alice_pubkey(s: &Secp256k1Pubkey) -> Result<PublicKey> {
        let bytes = hex::decode(&s.0)
            .map_err(|e| RefereeError::Musig2(format!("alice pubkey hex: {e}")))?;
        PublicKey::from_slice(&bytes)
            .map_err(|e| RefereeError::Musig2(format!("alice pubkey: {e}")))
    }

    fn parse_alice_pub_nonce(n: &PubNonce) -> Result<MusigPubNonce> {
        let bytes =
            hex::decode(&n.0).map_err(|e| RefereeError::Musig2(format!("alice nonce hex: {e}")))?;
        MusigPubNonce::from_bytes(&bytes)
            .map_err(|e| RefereeError::Musig2(format!("alice nonce: {e}")))
    }

    #[async_trait]
    impl Musig2Signer for RealSigner {
        fn pubshare(&self) -> Secp256k1Pubkey {
            Secp256k1Pubkey(hex::encode(self.pk.serialize()))
        }

        async fn begin_session(&self, swap_id: &SwapId, session: Session) -> Result<PubNonce> {
            let key = (swap_id.0.clone(), session);
            let mut sessions = self.sessions.lock().await;
            if sessions.contains_key(&key) {
                return Err(RefereeError::Musig2(format!(
                    "session already begun: {} / {:?}",
                    swap_id.0, session
                )));
            }
            // Fresh secret nonce per session — never reused.
            let mut seed = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut seed);
            let sec_nonce = SecNonce::build(seed).with_pubkey(self.pk).build();
            let pub_nonce = sec_nonce.public_nonce();
            let pub_hex = hex::encode(pub_nonce.serialize());
            sessions.insert(
                key,
                LiveSession {
                    sec_nonce,
                    pub_nonce,
                },
            );
            Ok(PubNonce(pub_hex))
        }

        async fn partial_sign(
            &self,
            swap_id: &SwapId,
            session: Session,
            tx_hash: &[u8],
            alice_pubshare: &Secp256k1Pubkey,
            alice_pub_nonce: &PubNonce,
        ) -> Result<PartialSig> {
            let key = (swap_id.0.clone(), session);
            let live = self.sessions.lock().await.remove(&key).ok_or_else(|| {
                RefereeError::Musig2(format!("no live session: {} / {:?}", swap_id.0, session))
            })?;
            let alice_pk = parse_alice_pubkey(alice_pubshare)?;
            let alice_nonce = parse_alice_pub_nonce(alice_pub_nonce)?;
            let key_agg = KeyAggContext::new([self.pk, alice_pk])
                .map_err(|e| RefereeError::Musig2(format!("keyagg: {e}")))?;
            let aggregated_nonce = ::musig2::AggNonce::sum([live.pub_nonce.clone(), alice_nonce]);
            let partial: PartialSignature = ::musig2::sign_partial(
                &key_agg,
                self.sk,
                live.sec_nonce,
                &aggregated_nonce,
                tx_hash,
            )
            .map_err(|e| RefereeError::Musig2(format!("partial sign: {e}")))?;
            Ok(PartialSig(hex::encode(partial.serialize())))
        }

        async fn discard_session(&self, swap_id: &SwapId, session: Session) -> Result<()> {
            self.sessions
                .lock()
                .await
                .remove(&(swap_id.0.clone(), session));
            Ok(())
        }
    }
}

#[cfg(feature = "musig2-real")]
pub use real_signer::RealSigner;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_session_lifecycle() {
        let s = MockSigner::new();
        let id = SwapId::fresh();
        let n = s.begin_session(&id, Session::Settle).await.unwrap();
        assert_eq!(n.0.len(), 128); // 64 hex chars × 2

        // Cannot begin twice in the same session.
        let err = s.begin_session(&id, Session::Settle).await.unwrap_err();
        assert!(matches!(err, RefereeError::Musig2(_)));

        // Sign consumes the session.
        let sig = s
            .partial_sign(
                &id,
                Session::Settle,
                b"tx",
                &Secp256k1Pubkey("02".to_string() + &"00".repeat(32)),
                &PubNonce("00".repeat(66)),
            )
            .await
            .unwrap();
        assert_eq!(sig.0.len(), 64);

        // Signing again with no live session errors.
        let err = s
            .partial_sign(
                &id,
                Session::Settle,
                b"tx",
                &Secp256k1Pubkey("02".to_string() + &"00".repeat(32)),
                &PubNonce("00".repeat(66)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RefereeError::Musig2(_)));
    }

    #[tokio::test]
    async fn discard_session_idempotent() {
        let s = MockSigner::new();
        let id = SwapId::fresh();
        s.begin_session(&id, Session::Refund).await.unwrap();
        s.discard_session(&id, Session::Refund).await.unwrap();
        s.discard_session(&id, Session::Refund).await.unwrap(); // ok
    }
}
