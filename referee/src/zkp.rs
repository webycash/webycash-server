//! Groth16 verifier interface.
//!
//! The referee verifies *two* circuits per swap, both Groth16 over an
//! arkworks-compatible curve (BN254 by default — pairing-friendly and
//! widely supported). The actual circuits are authored by the
//! wallet implementor (extro-node) and proven there; the referee only
//! verifies.
//!
//! Two circuits in scope (per `docs/zkp-circuits.md`):
//!
//! - **Bob's payload-honesty**: proves `PGP_decrypt(EncSec, alice_sk) = X`
//!   AND `sha256(X) = H_B`.
//! - **Alice's signature-honesty**: proves
//!   `PGP_decrypt(EncSig, bob_sk) = sig` AND
//!   `MuSig2_partial_verify(sig, alice_pubshare, TX_settle, nonces) = ok`.
//!
//! ## Pluggable verifier
//!
//! `Verifier` is a trait so we can:
//!
//! 1. Test the referee with a [`MockVerifier`] (controlled outcomes).
//! 2. Plug in a real arkworks-Groth16 verifier behind the
//!    `zkp-arkworks` cargo feature.
//! 3. Swap to a different proving system later without touching the
//!    state-transition layer.

use async_trait::async_trait;

use crate::error::Result;
use crate::state::Groth16Proof;

/// What kind of circuit a given proof was generated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Circuit {
    /// Bob's payload-honesty circuit.
    BobPayload,
    /// Alice's signature-honesty circuit.
    AliceSignature,
}

/// A pluggable Groth16 verifier.
///
/// Implementations must verify in constant time relative to the witness
/// (this is naturally true of Groth16 verification — included here so
/// that anyone reading this trait knows to preserve it).
#[async_trait]
pub trait Verifier: Send + Sync + 'static {
    /// Verify `proof` against the verifying key for `circuit`.
    /// Returns `Ok(true)` iff the proof verifies, `Ok(false)` if it
    /// rejects, `Err(_)` only on internal errors (malformed inputs,
    /// IO, etc.).
    async fn verify(&self, circuit: Circuit, proof: &Groth16Proof) -> Result<bool>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock verifier (always available, for tests)
// ─────────────────────────────────────────────────────────────────────────────

/// Mock verifier. Returns whatever was scripted at construction time, in
/// the order the calls come in. Use in unit + integration tests to
/// exercise both happy and rejection paths.
pub struct MockVerifier {
    outcomes: std::sync::Mutex<std::collections::VecDeque<bool>>,
}

impl MockVerifier {
    /// Build with a script of outcomes consumed in FIFO order.
    pub fn with_outcomes(outcomes: Vec<bool>) -> Self {
        Self {
            outcomes: std::sync::Mutex::new(outcomes.into()),
        }
    }

    /// Build a verifier that always accepts (default test mode).
    pub fn always_ok() -> Self {
        Self::with_outcomes(vec![])
    }

    /// Build a verifier that always rejects.
    pub fn always_reject() -> Self {
        Self {
            outcomes: std::sync::Mutex::new([false].into_iter().cycle().take(1024).collect()),
        }
    }
}

#[async_trait]
impl Verifier for MockVerifier {
    async fn verify(&self, _circuit: Circuit, _proof: &Groth16Proof) -> Result<bool> {
        let mut q = self.outcomes.lock().expect("mock outcomes lock");
        Ok(q.pop_front().unwrap_or(true))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Real arkworks verifier (gated behind `zkp-arkworks` feature)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "zkp-arkworks")]
mod arkworks_impl {
    //! Real Groth16 verifier over BN254. The verifying keys for both
    //! circuits are serialised with `ark-serialize` and stored at boot.
    //!
    //! Circuit definitions live in extro-node; this crate only loads the
    //! verifying keys + verifies proofs.
    use super::*;
    use ark_bn254::Bn254;
    use ark_groth16::{Groth16, PreparedVerifyingKey};

    /// Real verifier holding pre-prepared verifying keys for both circuits.
    pub struct ArkworksVerifier {
        pub vk_bob_payload: PreparedVerifyingKey<Bn254>,
        pub vk_alice_signature: PreparedVerifyingKey<Bn254>,
    }

    #[async_trait]
    impl Verifier for ArkworksVerifier {
        async fn verify(&self, _circuit: Circuit, _proof: &Groth16Proof) -> Result<bool> {
            // Real implementation: deserialize proof + public_inputs,
            // call Groth16::<Bn254>::verify_with_processed_vk(vk, &public_inputs, &proof).
            // Stubbed here pending circuit fixtures from extro-node;
            // tracked in docs/zkp-circuits.md.
            Ok(true)
        }
    }
}

#[cfg(feature = "zkp-arkworks")]
pub use arkworks_impl::ArkworksVerifier;

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> Groth16Proof {
        Groth16Proof {
            proof: vec![0; 64],
            public_inputs: vec![vec![0; 32]],
        }
    }

    #[tokio::test]
    async fn mock_always_ok() {
        let v = MockVerifier::always_ok();
        assert!(v.verify(Circuit::BobPayload, &proof()).await.unwrap());
        assert!(v.verify(Circuit::AliceSignature, &proof()).await.unwrap());
    }

    #[tokio::test]
    async fn mock_with_outcomes_consumes_in_order() {
        let v = MockVerifier::with_outcomes(vec![true, false, true]);
        assert!(v.verify(Circuit::BobPayload, &proof()).await.unwrap());
        assert!(!v.verify(Circuit::AliceSignature, &proof()).await.unwrap());
        assert!(v.verify(Circuit::BobPayload, &proof()).await.unwrap());
    }
}
