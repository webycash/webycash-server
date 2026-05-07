//! HTLC predicate execution entry point.
//!
//! The server enforces HTLC conditions with pure Rust ([`super::predicate`]).
//! AluVM contract validation is client-side only (webylib WASM).

use super::predicate::{evaluate, PredicateError, PredicateResult};
use super::state::{HtlcState, HtlcWitness};

/// Evaluate the HTLC predicate and return the verdict.
///
/// AluVM contract validation is client-side only (webylib WASM). The server
/// enforces HTLC conditions via the pure-Rust predicate in [`super::predicate`];
/// this function is the stable call site used by `asset_rgb`'s `/replace` handler.
pub fn execute_predicate(
    state: &HtlcState,
    witness: &HtlcWitness,
    current_unix: u64,
) -> Result<PredicateResult, PredicateError> {
    evaluate(state, witness, current_unix)
}

#[cfg(test)]
mod tests {
    use super::super::state::sha256_hex_of_ascii;
    use super::*;
    use sha2::{Digest, Sha256};

    fn fx() -> (HtlcState, HtlcWitness, u64) {
        let x_hex = "11".repeat(32);
        let committed = hex::encode(Sha256::digest(x_hex.as_bytes()));
        let claim_secret = "a".repeat(64);
        let refund_secret = "b".repeat(64);
        let state = HtlcState {
            committed_h_hex: committed,
            refund_after_unix: 1_714_003_200,
            claim_owner_hash_hex: sha256_hex_of_ascii(&claim_secret),
            refund_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        let witness = HtlcWitness {
            provided_x_hex: Some(x_hex),
            output_owner_hash_hex: sha256_hex_of_ascii(&claim_secret),
        };
        (state, witness, 1_714_003_100)
    }

    #[test]
    fn accepts_valid_claim() {
        let (s, w, t) = fx();
        assert_eq!(execute_predicate(&s, &w, t), Ok(PredicateResult::Claim));
    }

    #[test]
    fn rejects_wrong_preimage() {
        let (s, mut w, t) = fx();
        w.provided_x_hex = Some("ff".repeat(32));
        assert_eq!(
            execute_predicate(&s, &w, t),
            Err(PredicateError::PreimageMismatch)
        );
    }

    #[test]
    fn accepts_refund_after_timeout() {
        let (s, _, _) = fx();
        let refund_secret = "b".repeat(64);
        let w = HtlcWitness {
            provided_x_hex: None,
            output_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        assert_eq!(
            execute_predicate(&s, &w, s.refund_after_unix + 1),
            Ok(PredicateResult::Refund)
        );
    }
}
