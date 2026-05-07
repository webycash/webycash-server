//! HTLC predicate evaluation.
//!
//! The pure, host-side check that decides whether an HTLC-locked output
//! can be replaced. Called from inside the AluVM execution path (see
//! [`super::vm`]), but written as a standalone Rust function so it's
//! easy to test, easy to audit, and easy to compile to WASM for
//! client-side verification.
//!
//! Evaluation rules (also documented in `docs/referee-zkp-based-swap.md`):
//!
//! ```text
//! accept iff
//!     (witness.provided_x is Some(X)
//!      AND sha256(hex(X)_ascii) == state.committed_h_hex
//!      AND witness.output_owner_hash_hex == state.claim_owner_hash_hex)
//!  OR
//!     (current_unix >= state.refund_after_unix
//!      AND witness.output_owner_hash_hex == state.refund_owner_hash_hex)
//! ```
//!
//! Notes:
//!
//! - We hash the **ASCII bytes of the hex form** of `X`, matching the
//!   secret-hashing convention used everywhere else in the system. This
//!   keeps the wire convention uniform — the same hash function works
//!   for `X` here as for token secrets in `to_public`.
//! - `current_unix` is the SERVER's clock when called from `/replace`.
//!   The wallet's clock is for its own pre-flight check; the server
//!   ignores any timestamp the wallet sends.
//! - Both branches require the `output_owner_hash_hex` match. The
//!   server reads this directly from the request's `htlc_witness` slot,
//!   not from the user's claim — and cross-checks it against the new
//!   output secret's actual hash before accepting (the wallet cannot
//!   lie about the output owner without being caught by the server's
//!   own hash computation).

use sha2::{Digest, Sha256};

use crate::asset_rgb::htlc::state::{HtlcState, HtlcWitness};

/// Verdict produced by [`evaluate`].
///
/// `Claim` and `Refund` are both accepts; the variant identifies which
/// path the predicate matched, which the host uses for diagnostics and
/// audit logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateResult {
    /// Claim path: preimage matched, output owner is claim_owner.
    Claim,
    /// Refund path: timeout passed, output owner is refund_owner.
    Refund,
}

/// Why an HTLC predicate evaluation rejected.
///
/// Each variant maps to a specific user-facing 422 diagnostic — the
/// server returns the variant's `Display` form so wallets can show
/// actionable error messages.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PredicateError {
    /// Provided preimage `X` does not hash to `committed_H`.
    #[error("htlc: provided preimage does not match committed hash")]
    PreimageMismatch,

    /// Caller took the claim path but `output_owner_hash` is not the
    /// pre-committed claim owner. Either the wrong recipient, or an
    /// attempt to redirect after seeing X.
    #[error("htlc: claim-path output owner mismatch")]
    ClaimOwnerMismatch,

    /// Caller tried to refund before `refund_after_unix`.
    #[error("htlc: refund window not yet open ({delta} seconds remaining)")]
    RefundLocked {
        /// Seconds until the refund window opens.
        delta: u64,
    },

    /// Caller took the refund path but `output_owner_hash` is not the
    /// pre-committed refund owner.
    #[error("htlc: refund-path output owner mismatch")]
    RefundOwnerMismatch,

    /// Witness has neither a preimage nor a refund-path-eligible time.
    /// (Equivalent to "neither branch can be satisfied by this witness".)
    #[error("htlc: witness satisfies neither claim nor refund path")]
    NeitherPath,

    /// Internal: hex decode of `provided_x_hex` failed. Bad input.
    #[error("htlc: malformed preimage hex: {0}")]
    BadPreimageHex(String),
}

/// Evaluate the predicate. `Ok(_)` accepts and identifies the path;
/// `Err(_)` rejects with a specific reason.
///
/// `current_unix` MUST be the server's authoritative clock. Wallets
/// pass their own clock for client-side dry-runs only.
pub fn evaluate(
    state: &HtlcState,
    witness: &HtlcWitness,
    current_unix: u64,
) -> Result<PredicateResult, PredicateError> {
    // Try the claim path first when a preimage is provided.
    if let Some(x_hex) = &witness.provided_x_hex {
        let _ = hex::decode(x_hex).map_err(|e| PredicateError::BadPreimageHex(format!("{e}")))?;

        // Hash the ASCII bytes of the hex form, matching the system-wide
        // secret-hashing convention.
        let computed = hex::encode(Sha256::digest(x_hex.as_bytes()));
        if computed != state.committed_h_hex {
            // Fall through to refund path: a wrong preimage shouldn't
            // be a hard reject if the timeout has also passed (the
            // wallet may have been confused). It's the OR semantics
            // of the HTLC predicate.
            if current_unix >= state.refund_after_unix
                && witness.output_owner_hash_hex == state.refund_owner_hash_hex
            {
                return Ok(PredicateResult::Refund);
            }
            return Err(PredicateError::PreimageMismatch);
        }
        if witness.output_owner_hash_hex != state.claim_owner_hash_hex {
            return Err(PredicateError::ClaimOwnerMismatch);
        }
        return Ok(PredicateResult::Claim);
    }

    // No preimage → must be the refund path.
    if current_unix < state.refund_after_unix {
        return Err(PredicateError::RefundLocked {
            delta: state.refund_after_unix - current_unix,
        });
    }
    if witness.output_owner_hash_hex != state.refund_owner_hash_hex {
        return Err(PredicateError::RefundOwnerMismatch);
    }
    Ok(PredicateResult::Refund)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_rgb::htlc::state::sha256_hex_of_ascii;

    /// Build a fixture: state with random-looking commitments and a
    /// matching witness for the claim path.
    fn fx_claim() -> (HtlcState, HtlcWitness, u64) {
        let x_hex = "11".repeat(32);
        let committed = hex::encode(Sha256::digest(x_hex.as_bytes()));
        let claim_secret = "a".repeat(64);
        let refund_secret = "b".repeat(64);
        let state = HtlcState {
            committed_h_hex: committed,
            refund_after_unix: 1714003200,
            claim_owner_hash_hex: sha256_hex_of_ascii(&claim_secret),
            refund_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        let witness = HtlcWitness {
            provided_x_hex: Some(x_hex),
            output_owner_hash_hex: sha256_hex_of_ascii(&claim_secret),
        };
        (state, witness, 1714003100) // before refund timeout
    }

    #[test]
    fn claim_path_accepts() {
        let (s, w, t) = fx_claim();
        assert_eq!(evaluate(&s, &w, t), Ok(PredicateResult::Claim));
    }

    #[test]
    fn claim_path_rejects_wrong_preimage() {
        let (s, mut w, t) = fx_claim();
        w.provided_x_hex = Some("ff".repeat(32));
        assert_eq!(evaluate(&s, &w, t), Err(PredicateError::PreimageMismatch));
    }

    #[test]
    fn claim_path_rejects_redirected_owner() {
        let (s, mut w, t) = fx_claim();
        w.output_owner_hash_hex = sha256_hex_of_ascii(&"c".repeat(64));
        assert_eq!(evaluate(&s, &w, t), Err(PredicateError::ClaimOwnerMismatch));
    }

    #[test]
    fn refund_path_accepts_after_timeout_with_correct_owner() {
        let (s, _claim_w, _) = fx_claim();
        let refund_secret = "b".repeat(64);
        let w = HtlcWitness {
            provided_x_hex: None,
            output_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        // Time exactly at the boundary is allowed.
        assert_eq!(
            evaluate(&s, &w, s.refund_after_unix),
            Ok(PredicateResult::Refund)
        );
        assert_eq!(
            evaluate(&s, &w, s.refund_after_unix + 1_000_000),
            Ok(PredicateResult::Refund)
        );
    }

    #[test]
    fn refund_path_rejects_before_timeout() {
        let (s, _, _) = fx_claim();
        let refund_secret = "b".repeat(64);
        let w = HtlcWitness {
            provided_x_hex: None,
            output_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        let err = evaluate(&s, &w, s.refund_after_unix - 100);
        match err {
            Err(PredicateError::RefundLocked { delta }) => assert_eq!(delta, 100),
            other => panic!("expected RefundLocked, got {other:?}"),
        }
    }

    #[test]
    fn refund_path_rejects_wrong_refund_owner() {
        let (s, _, _) = fx_claim();
        let w = HtlcWitness {
            provided_x_hex: None,
            output_owner_hash_hex: sha256_hex_of_ascii(&"x".repeat(64)),
        };
        assert_eq!(
            evaluate(&s, &w, s.refund_after_unix + 1),
            Err(PredicateError::RefundOwnerMismatch)
        );
    }

    /// If the wallet provides a wrong preimage but the timeout HAS
    /// passed and the refund-owner happens to match, the OR-semantics
    /// of the HTLC let it through as a refund. Documented behaviour.
    #[test]
    fn wrong_preimage_after_timeout_falls_through_to_refund() {
        let (s, mut w, _) = fx_claim();
        w.provided_x_hex = Some("ff".repeat(32));
        let refund_secret = "b".repeat(64);
        w.output_owner_hash_hex = sha256_hex_of_ascii(&refund_secret);
        assert_eq!(
            evaluate(&s, &w, s.refund_after_unix + 1),
            Ok(PredicateResult::Refund)
        );
    }

    #[test]
    fn rejects_malformed_preimage_hex() {
        let (s, mut w, t) = fx_claim();
        w.provided_x_hex = Some("zz".repeat(32));
        assert!(matches!(
            evaluate(&s, &w, t),
            Err(PredicateError::BadPreimageHex(_))
        ));
    }
}
