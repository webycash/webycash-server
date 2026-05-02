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
    //! circuits are serialised with `ark-serialize` (canonical
    //! compressed form) and loaded at boot.
    //!
    //! Circuit definitions live in extro-node; this crate only loads
    //! the verifying keys + verifies proofs. The proof bytes and
    //! public inputs in [`Groth16Proof`] are interpreted as
    //! `ark-serialize` canonical encodings; mismatches cause
    //! verification to fail with `RefereeError::ZkpRejected` at
    //! runtime.
    use super::*;
    use crate::error::RefereeError;
    use ark_bn254::{Bn254, Fr};
    use ark_ff::PrimeField;
    use ark_groth16::{Groth16, PreparedVerifyingKey, Proof, VerifyingKey};
    use ark_serialize::CanonicalDeserialize;
    use std::path::Path;

    /// Real verifier holding pre-prepared verifying keys for both circuits.
    pub struct ArkworksVerifier {
        /// Prepared VK for Bob's payload-honesty circuit.
        pub vk_bob_payload: PreparedVerifyingKey<Bn254>,
        /// Prepared VK for Alice's signature-honesty circuit.
        pub vk_alice_signature: PreparedVerifyingKey<Bn254>,
    }

    impl ArkworksVerifier {
        /// Load the two verifying-key fixtures from disk. Each file
        /// must contain the canonical-compressed serialisation of
        /// `ark_groth16::VerifyingKey<Bn254>` produced by
        /// extro-node's circuit setup. See
        /// `webycash-server/referee/docs/zkp-circuits.md`.
        pub fn load_from_files(
            vk_bob_path: impl AsRef<Path>,
            vk_alice_path: impl AsRef<Path>,
        ) -> Result<Self> {
            let vk_bob = load_vk(vk_bob_path.as_ref())?;
            let vk_alice = load_vk(vk_alice_path.as_ref())?;
            Ok(Self {
                vk_bob_payload: PreparedVerifyingKey::from(vk_bob),
                vk_alice_signature: PreparedVerifyingKey::from(vk_alice),
            })
        }

        fn vk_for(&self, circuit: Circuit) -> &PreparedVerifyingKey<Bn254> {
            match circuit {
                Circuit::BobPayload => &self.vk_bob_payload,
                Circuit::AliceSignature => &self.vk_alice_signature,
            }
        }
    }

    fn load_vk(path: &Path) -> Result<VerifyingKey<Bn254>> {
        let bytes = std::fs::read(path)
            .map_err(|e| RefereeError::Crypto(format!("vk read {}: {e}", path.display())))?;
        VerifyingKey::<Bn254>::deserialize_compressed(&*bytes)
            .map_err(|e| RefereeError::Crypto(format!("vk decode {}: {e}", path.display())))
    }

    fn decode_proof(bytes: &[u8]) -> Result<Proof<Bn254>> {
        Proof::<Bn254>::deserialize_compressed(bytes)
            .map_err(|e| RefereeError::Crypto(format!("proof decode: {e}")))
    }

    fn decode_public_inputs(inputs: &[Vec<u8>]) -> Result<Vec<Fr>> {
        // Each public input is the big-endian encoding of an Fr
        // element. We interpret them as little-endian here only if
        // extro-node agrees — for now we use BE since that matches
        // the circuit-fixture convention extro-node will produce.
        inputs
            .iter()
            .map(|bytes| {
                if bytes.len() > 32 {
                    return Err(RefereeError::Crypto(
                        "public input larger than 32 bytes".into(),
                    ));
                }
                Ok(Fr::from_be_bytes_mod_order(bytes))
            })
            .collect()
    }

    #[async_trait]
    impl Verifier for ArkworksVerifier {
        async fn verify(&self, circuit: Circuit, proof: &Groth16Proof) -> Result<bool> {
            let pvk = self.vk_for(circuit);
            let p = decode_proof(&proof.proof)?;
            let inputs = decode_public_inputs(&proof.public_inputs)?;
            Groth16::<Bn254>::verify_proof(pvk, &p, &inputs)
                .map_err(|e| RefereeError::Crypto(format!("groth16 verify: {e}")))
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
