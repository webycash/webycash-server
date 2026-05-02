//! Top-level transaction model: the user-facing shape of a swap row.
//!
//! Every persisted swap has two faces:
//!
//! - The [`Transaction`] — explicit, indexable, serialisable as flat
//!   attributes on every storage backend. This is what the
//!   `/v1/parties/{pgp_fp}/swaps` history endpoint returns and what
//!   operators inspect.
//! - The opaque `state_blob` (canonical JSON of `SwapState<P>`) —
//!   the orchestrator's internal continuation. Held as one attribute
//!   alongside the `Transaction` fields; meaningful only to
//!   [`crate::api::orchestrator`].
//!
//! Both are persisted in the same row. Splitting the user-facing
//! fields out gives DynamoDB GSIs something to index, gives Redis
//! sorted-sets something to project, and gives the operator a row
//! they can read at a glance — instead of an opaque JSON blob.
//!
//! `Transaction` is [`derive`d](Transaction::derive_from) from a
//! `SwapState<P>` plus the current phase + status. The projection is
//! a pure function so persistence layer code never duplicates the
//! field-extraction logic.

use serde::{Deserialize, Serialize};

use crate::state::{
    AnyPhaseSwapState, ArkOutpointHash, PgpFingerprint, Phase, SwapId, SwapState, WebcashPublicHash,
};

/// User-visible status. A coarse projection of [`crate::state::phases`];
/// see `docs/transaction-model.md` for the full mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionStatus {
    /// Swap is in flight (any non-terminal phase, including transient
    /// `aborted` / `invalidated` on the way to `refunded`).
    Pending,
    /// Settled successfully — Bob has the release-settle, Alice's vtxo
    /// is co-signed for transfer.
    Settled,
    /// Refunded — abort path completed, Alice has her refund partial.
    Refunded,
    /// Canceled by a party (or by the referee at swap_max_age timeout).
    Canceled,
}

impl TransactionStatus {
    /// Coarse projection of the typestate phase name.
    pub fn for_phase(phase: &str) -> Self {
        match phase {
            "settled" => TransactionStatus::Settled,
            "refunded" => TransactionStatus::Refunded,
            "canceled" => TransactionStatus::Canceled,
            _ => TransactionStatus::Pending,
        }
    }

    /// Whether this is a terminal status. Terminal rows are immutable
    /// from the orchestrator's perspective.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, TransactionStatus::Pending)
    }

    /// Stable string used in DynamoDB / Redis attributes and over the
    /// wire. Inverse of [`Self::for_phase`] only when the phase is
    /// itself terminal — `pending` is not a phase name.
    pub const fn as_str(self) -> &'static str {
        match self {
            TransactionStatus::Pending => "pending",
            TransactionStatus::Settled => "settled",
            TransactionStatus::Refunded => "refunded",
            TransactionStatus::Canceled => "canceled",
        }
    }
}

/// The participating role of a `pgp_fp` in a transaction. Returned
/// from the history endpoint so the wallet knows which side of the
/// swap it was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartyRole {
    /// The fingerprint matched `bob_pgp_fp` (webcash holder).
    Bob,
    /// The fingerprint matched `alice_pgp_fp` (ARK vtxo holder).
    Alice,
    /// The fingerprint matched both — self-swap.
    Both,
}

/// The user-facing shape of a swap, persisted as flat attributes on
/// every backend. See `docs/transaction-model.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// Stable swap id. Partition key on every backend.
    pub swap_id: SwapId,
    /// Coarse user-facing status.
    pub status: TransactionStatus,
    /// Detailed typestate phase.
    pub phase: String,
    /// `true` when [`status`](Self::status) is terminal.
    pub terminal: bool,
    /// Bob's PGP fingerprint (webcash holder).
    pub bob_pgp_fp: PgpFingerprint,
    /// Alice's PGP fingerprint (ARK vtxo holder).
    pub alice_pgp_fp: PgpFingerprint,
    /// `H_B = sha256(S_B)` — the public hash on the webcash leg.
    pub webcash_public_hash: WebcashPublicHash,
    /// Hash of the ARK vtxo being mediated.
    pub vtxo_outpoint_hash: ArkOutpointHash,
    /// What Alice's MuSig2 partial signs over on the settle path.
    pub tx_settle_hash: String,
    /// What Alice's MuSig2 partial signs over on the refund path.
    pub tx_refund_hash: String,
    /// Wall-clock at `init` (Unix seconds).
    pub created_at_unix: u64,
    /// Wall-clock at last phase transition (Unix seconds).
    pub updated_at_unix: u64,
    /// Bounded by `Config::insert_push_retry`.
    pub insert_push_attempts: u8,
    /// Free-text user-provided reason. Set when `status = canceled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
    /// Which party initiated the cancel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled_by_pgp_fp: Option<PgpFingerprint>,
    /// RGB contract id of the timeout-bound backup record (see
    /// `docs/transaction-model.md` §HTLC backup refund). Populated at
    /// initiate when the RGB server is reachable; `None` otherwise so
    /// the swap can still proceed (the MuSig2 refund path is the
    /// primary mechanism).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub htlc_refund_contract_id: Option<String>,
    /// Opaque continuation for the orchestrator. Canonical JSON of
    /// `SwapState<P>` for the current phase. NEVER inspected by the
    /// API surface.
    pub state_blob: AnyPhaseSwapState,
}

impl Transaction {
    /// Project a phase-typed state into the user-facing transaction
    /// shape. Pure: no I/O, no clock — caller passes `now_unix` for
    /// `updated_at_unix`. The `phase` argument is the [`Phase::NAME`]
    /// of `P`; we accept it explicitly so the same projection works
    /// for the `Canceled` phase (which doesn't go through the typestate
    /// `advance` chain — see [`crate::state::transitions`] notes).
    pub fn derive_from<P: Phase>(
        s: &SwapState<P>,
        phase: &str,
        now_unix: u64,
        cancel_reason: Option<String>,
        canceled_by_pgp_fp: Option<PgpFingerprint>,
        htlc_refund_contract_id: Option<String>,
    ) -> Self {
        let inner = serde_json::to_value(s).expect("SwapState always serialises");
        let status = TransactionStatus::for_phase(phase);
        Self {
            swap_id: s.id.clone(),
            status,
            phase: phase.into(),
            terminal: status.is_terminal(),
            bob_pgp_fp: s.parties.bob_pgp_fp.clone(),
            alice_pgp_fp: s.parties.alice_pgp_fp.clone(),
            webcash_public_hash: s.bob.h_b.clone(),
            vtxo_outpoint_hash: s.alice.vtxo.clone(),
            tx_settle_hash: s.alice.tx_settle_hash.clone(),
            tx_refund_hash: s.alice.tx_refund_hash.clone(),
            created_at_unix: s.phase_entered_at, // overridden by store on first write
            updated_at_unix: now_unix,
            insert_push_attempts: s.insert_push_attempts,
            cancel_reason,
            canceled_by_pgp_fp,
            htlc_refund_contract_id,
            state_blob: AnyPhaseSwapState {
                phase: phase.into(),
                inner,
            },
        }
    }

    /// Compact view used for history listings — drops the state-blob
    /// and the cancel/HTLC details that aren't needed for the index.
    pub fn summary(&self, role: PartyRole) -> TransactionSummary {
        TransactionSummary {
            swap_id: self.swap_id.clone(),
            status: self.status,
            phase: self.phase.clone(),
            terminal: self.terminal,
            bob_pgp_fp: self.bob_pgp_fp.clone(),
            alice_pgp_fp: self.alice_pgp_fp.clone(),
            role,
            created_at_unix: self.created_at_unix,
            updated_at_unix: self.updated_at_unix,
        }
    }
}

/// Compact transaction shape returned from
/// `GET /v1/parties/{pgp_fp}/swaps`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionSummary {
    /// Stable swap id.
    pub swap_id: SwapId,
    /// Coarse user-facing status.
    pub status: TransactionStatus,
    /// Detailed typestate phase.
    pub phase: String,
    /// `true` when status is terminal.
    pub terminal: bool,
    /// Bob's PGP fingerprint.
    pub bob_pgp_fp: PgpFingerprint,
    /// Alice's PGP fingerprint.
    pub alice_pgp_fp: PgpFingerprint,
    /// Which side the queried fingerprint was on.
    pub role: PartyRole,
    /// Unix seconds of `init`.
    pub created_at_unix: u64,
    /// Unix seconds of last phase transition.
    pub updated_at_unix: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AliceMusig2Nonces, AlicePayload, BobPayload, Groth16Proof, Musig2Sessions, Parties,
        PgpEncrypted, Secp256k1Pubkey,
    };

    fn fresh() -> SwapState<crate::state::SwapInit> {
        let id = SwapId::fresh();
        SwapState {
            id: id.clone(),
            parties: Parties {
                bob_pgp_fp: PgpFingerprint("aa".repeat(20)),
                bob_pgp_pubkey_hex: "bb".repeat(64),
                alice_pgp_fp: PgpFingerprint("cc".repeat(20)),
                alice_pgp_pubkey_hex: "dd".repeat(64),
                alice_musig2_pubkey: Secp256k1Pubkey("02".to_string() + &"ee".repeat(32)),
                bob_cancel_pubkey_hex: "11".repeat(32),
                alice_cancel_pubkey_hex: "22".repeat(32),
            },
            bob: BobPayload {
                h_b: WebcashPublicHash::new("h".repeat(64)),
                enc_secret_for_alice: PgpEncrypted::new(b"ct".to_vec()),
                zkp_payload: Groth16Proof {
                    proof: vec![1],
                    public_inputs: vec![vec![0xa]],
                },
            },
            alice: AlicePayload {
                vtxo: ArkOutpointHash("v".repeat(64)),
                tx_settle_hash: "s".repeat(64),
                tx_refund_hash: "r".repeat(64),
                enc_partial_sig_for_bob: PgpEncrypted::new(b"ct".to_vec()),
                zkp_signature: Groth16Proof {
                    proof: vec![2],
                    public_inputs: vec![vec![0xb]],
                },
            },
            alice_nonces: AliceMusig2Nonces {
                settle_nonce_pub: "11".repeat(66),
                refund_nonce_pub: "22".repeat(66),
            },
            referee_sessions: Musig2Sessions {
                settle_pub_nonce: "33".repeat(66),
                refund_pub_nonce: "44".repeat(66),
                secret_nonce_handle: id,
            },
            phase_entered_at: 1000,
            insert_push_attempts: 0,
            audit_tip_hex: "00".repeat(32),
            _phase: std::marker::PhantomData,
        }
    }

    #[test]
    fn status_for_phase_maps_terminal_phases() {
        assert_eq!(
            TransactionStatus::for_phase("settled"),
            TransactionStatus::Settled
        );
        assert_eq!(
            TransactionStatus::for_phase("refunded"),
            TransactionStatus::Refunded
        );
        assert_eq!(
            TransactionStatus::for_phase("canceled"),
            TransactionStatus::Canceled
        );
        assert_eq!(
            TransactionStatus::for_phase("init"),
            TransactionStatus::Pending
        );
        assert_eq!(
            TransactionStatus::for_phase("insert-pushed"),
            TransactionStatus::Pending
        );
    }

    #[test]
    fn derive_projects_top_level_fields() {
        let s = fresh();
        let tx = Transaction::derive_from(&s, "init", 1234, None, None, None);
        assert_eq!(tx.swap_id, s.id);
        assert_eq!(tx.status, TransactionStatus::Pending);
        assert_eq!(tx.phase, "init");
        assert!(!tx.terminal);
        assert_eq!(tx.bob_pgp_fp.0, "aa".repeat(20));
        assert_eq!(tx.alice_pgp_fp.0, "cc".repeat(20));
        assert_eq!(tx.updated_at_unix, 1234);
    }

    #[test]
    fn derive_terminal_phase_sets_terminal_true() {
        let s = fresh();
        let tx = Transaction::derive_from(&s, "settled", 1, None, None, None);
        assert!(tx.terminal);
        assert_eq!(tx.status, TransactionStatus::Settled);
    }

    #[test]
    fn summary_records_role() {
        let s = fresh();
        let tx = Transaction::derive_from(&s, "init", 1, None, None, None);
        let bob_view = tx.summary(PartyRole::Bob);
        assert!(matches!(bob_view.role, PartyRole::Bob));
        assert_eq!(bob_view.swap_id, tx.swap_id);
    }
}
