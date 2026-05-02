//! Wire-format state + witness types for the HTLC schema.
//!
//! These types travel:
//! - [`HtlcState`] is encoded into the record metadata of an output that's
//!   in HTLC-locked state. The server stores it alongside the rest of the
//!   record fields; it's not on the secret/public wire form.
//! - [`HtlcWitness`] travels in the `htlc_witness` slot of a `/replace`
//!   request body, alongside `webcashes` / `new_webcashes` / `legalese`.
//!
//! Hex-string encoding throughout for line-protocol simplicity. Strict-types
//! schemas land alongside the rest of the RGB ecosystem integration; for
//! v1 these are serde-derived JSON.

use serde::{Deserialize, Serialize};

/// State carried in an HTLC-locked output's record metadata.
///
/// The fields are committed at the moment the lock is created (i.e. when
/// some prior `/replace` produced this output in HTLC state). Once an
/// output carries this state, the only valid `/replace` of it is one
/// satisfying the predicate in [`super::predicate::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtlcState {
    /// 64-char hex of `sha256(X)` where `X` is the secret preimage that
    /// will be revealed by whoever takes the claim path. Pre-committed
    /// at lock time; cannot be changed.
    pub committed_h_hex: String,

    /// Unix timestamp (seconds) at which the refund path becomes
    /// available. Before this, only the claim path is valid; from this
    /// timestamp on, the refund path is also valid.
    pub refund_after_unix: u64,

    /// 64-char hex of `sha256(claim_owner_secret_ascii_hex)`. The
    /// claim-path output's hash MUST equal this. Pre-committed at lock
    /// time; ensures the preimage X cannot be redirected to a different
    /// recipient.
    pub claim_owner_hash_hex: String,

    /// 64-char hex of `sha256(refund_owner_secret_ascii_hex)`. The
    /// refund-path output's hash MUST equal this. Pre-committed at lock
    /// time; ensures only the original locker can refund.
    pub refund_owner_hash_hex: String,
}

/// Witness data attached to a `/replace` request that targets an
/// HTLC-locked input. The wallet provides this; the server verifies via
/// the AluVM-gated [`super::predicate::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtlcWitness {
    /// Hex-encoded preimage when taking the claim path; empty / absent
    /// when taking the refund path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provided_x_hex: Option<String>,

    /// Hex of `sha256(secret_hex_ascii_bytes)` for the *output* secret
    /// the replace is producing. The server compares this to the
    /// state's `claim_owner_hash_hex` (claim path) or
    /// `refund_owner_hash_hex` (refund path) without trusting the
    /// wallet to do the match itself. Same hash function as the rest
    /// of the system.
    pub output_owner_hash_hex: String,
}

impl HtlcState {
    /// Build a state for the *standard* swap construction, given:
    /// - the agreed preimage hash `H`
    /// - a refund deadline (unix seconds, in **server time**)
    /// - the secret the *recipient* will use after receiving X
    /// - the secret the *sender* will use to refund after the timeout
    ///
    /// **Time discipline.** The `refund_after_unix` here is whatever the
    /// caller stamps in. When this state is built as part of a `/replace`
    /// that locks an output, the *server* must construct it from
    /// `server_now + caller_supplied_delta` — never trusting the wallet
    /// to supply an absolute timestamp. See [`LockRequest::stamp_into_state`].
    pub fn for_swap(
        committed_h_hex: impl Into<String>,
        refund_after_unix: u64,
        claim_owner_secret_hex: &str,
        refund_owner_secret_hex: &str,
    ) -> Self {
        Self {
            committed_h_hex: committed_h_hex.into(),
            refund_after_unix,
            claim_owner_hash_hex: sha256_hex_of_ascii(claim_owner_secret_hex),
            refund_owner_hash_hex: sha256_hex_of_ascii(refund_owner_secret_hex),
        }
    }
}

/// Wallet-supplied request to lock an output into HTLC state. The caller
/// gives us a *delta* from server-now, never an absolute timestamp —
/// preventing a malicious / desynced wallet from stamping a stale or
/// distant-future deadline into the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRequest {
    /// Hex of `sha256(X)` — the preimage commitment.
    pub committed_h_hex: String,
    /// Seconds **from server-now** until refund unlocks. The server
    /// adds this to its own clock when building the resulting
    /// [`HtlcState`].
    pub refund_after_seconds_from_now: u64,
    /// Pre-committed claim-path owner. The server hashes this into
    /// `claim_owner_hash_hex` itself rather than trusting the wallet.
    pub claim_owner_secret_hex: String,
    /// Pre-committed refund-path owner.
    pub refund_owner_secret_hex: String,
}

impl LockRequest {
    /// Stamp the request into a server-clock-anchored [`HtlcState`].
    ///
    /// `server_now_unix` MUST be the server's authoritative clock. The
    /// wallet has no input to this number.
    pub fn stamp_into_state(&self, server_now_unix: u64) -> HtlcState {
        HtlcState {
            committed_h_hex: self.committed_h_hex.clone(),
            refund_after_unix: server_now_unix
                .saturating_add(self.refund_after_seconds_from_now),
            claim_owner_hash_hex: sha256_hex_of_ascii(&self.claim_owner_secret_hex),
            refund_owner_hash_hex: sha256_hex_of_ascii(&self.refund_owner_secret_hex),
        }
    }
}

/// Hash function consistent with the rest of the system: SHA256 over the
/// ASCII bytes of the secret hex string. Same as
/// `webylib_core::ops::recover::sha256_hex_of_ascii` and the server's
/// per-asset `to_public`.
pub(crate) fn sha256_hex_of_ascii(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_swap_builds_consistent_owner_hashes() {
        let claim = "a".repeat(64);
        let refund = "b".repeat(64);
        let s = HtlcState::for_swap(
            "00".repeat(32),
            1714003200,
            &claim,
            &refund,
        );
        assert_eq!(s.claim_owner_hash_hex, sha256_hex_of_ascii(&claim));
        assert_eq!(s.refund_owner_hash_hex, sha256_hex_of_ascii(&refund));
        assert_ne!(s.claim_owner_hash_hex, s.refund_owner_hash_hex);
    }

    #[test]
    fn lock_request_stamps_with_server_clock() {
        let req = LockRequest {
            committed_h_hex: "00".repeat(32),
            refund_after_seconds_from_now: 1800,
            claim_owner_secret_hex: "a".repeat(64),
            refund_owner_secret_hex: "b".repeat(64),
        };
        let server_now = 1_714_003_200u64;
        let s = req.stamp_into_state(server_now);
        assert_eq!(s.refund_after_unix, server_now + 1800);
        // Owner hashes computed from the request's secrets, not trusted from wallet.
        assert_eq!(s.claim_owner_hash_hex, sha256_hex_of_ascii(&req.claim_owner_secret_hex));
        assert_eq!(s.refund_owner_hash_hex, sha256_hex_of_ascii(&req.refund_owner_secret_hex));
    }

    #[test]
    fn lock_request_saturates_on_overflow() {
        let req = LockRequest {
            committed_h_hex: "00".repeat(32),
            refund_after_seconds_from_now: u64::MAX,
            claim_owner_secret_hex: "a".repeat(64),
            refund_owner_secret_hex: "b".repeat(64),
        };
        let s = req.stamp_into_state(100);
        assert_eq!(s.refund_after_unix, u64::MAX);
    }

    #[test]
    fn json_roundtrip() {
        let s = HtlcState {
            committed_h_hex: "11".repeat(32),
            refund_after_unix: 12345,
            claim_owner_hash_hex: "22".repeat(32),
            refund_owner_hash_hex: "33".repeat(32),
        };
        let s2: HtlcState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, s2);

        let w = HtlcWitness {
            provided_x_hex: Some("44".repeat(32)),
            output_owner_hash_hex: "55".repeat(32),
        };
        let w2: HtlcWitness = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert_eq!(w, w2);

        let refund = HtlcWitness {
            provided_x_hex: None,
            output_owner_hash_hex: "66".repeat(32),
        };
        let refund2: HtlcWitness =
            serde_json::from_str(&serde_json::to_string(&refund).unwrap()).unwrap();
        assert_eq!(refund, refund2);
    }
}
