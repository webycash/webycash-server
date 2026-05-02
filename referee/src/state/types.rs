//! Data carried inside every `SwapState<P>`.
//!
//! All fields are `Clone + Serialize + Deserialize` so the whole state
//! can be persisted as canonical JSON in the audit log + Postgres. The
//! types below are intentionally newtypes around `Vec<u8>` / `String`
//! so their meaning at the call site is unambiguous:
//!
//! - `PgpEncrypted<T>` — opaque bytes the referee NEVER decrypts. The
//!   phantom `T` records what the cleartext is *supposed* to be (a
//!   webcash secret, an Alice MuSig2 partial-sig, …) so the type
//!   system can keep them distinct without revealing cleartext.
//! - `Groth16Proof` — the proof bytes only; public inputs are carried
//!   alongside.
//! - `WebcashPublicHash` / `ArkOutpointHash` — string newtypes so
//!   accidentally swapping the two is a type error.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::sign::Tag;

// ─────────────────────────────────────────────────────────────────────────────
// Cleartext-resistant newtypes
// ─────────────────────────────────────────────────────────────────────────────

/// Opaque ciphertext. The referee verifies *honesty* via a ZKP; the
/// referee NEVER decrypts. The phantom records what the cleartext
/// would be at the recipient.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgpEncrypted<T> {
    /// Raw ciphertext bytes (PGP message body, base64 over the wire).
    pub bytes: Vec<u8>,
    #[serde(skip)]
    _phantom: PhantomData<T>,
}

impl<T> PgpEncrypted<T> {
    /// Wrap raw ciphertext bytes. Caller asserts (with a ZKP that follows)
    /// that the bytes encrypt to the right cleartext under the right pubkey.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            _phantom: PhantomData,
        }
    }
}

/// Phantom marker for cleartext that *would be* a webcash secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebcashSecret;

/// Phantom marker for cleartext that *would be* Alice's MuSig2
/// partial-signature on `TX_settle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliceTxSettlePartialSig;

/// Hex of `sha256(secret)` on the webcash leg.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebcashPublicHash(pub String);

impl WebcashPublicHash {
    /// Construct without validation; caller is expected to have validated.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Hash of an ARK vtxo outpoint. Used in the swap envelope so the audit
/// log records *which* vtxo was being mediated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArkOutpointHash(pub String);

/// Hex of an Ed25519 fingerprint (lower-case, 40 chars).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PgpFingerprint(pub String);

/// Hex of a 32-byte Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ed25519Pubkey(pub String);

/// Hex of a 33-byte secp256k1 compressed pubkey (MuSig2 share).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Secp256k1Pubkey(pub String);

// ─────────────────────────────────────────────────────────────────────────────
// Identifiers
// ─────────────────────────────────────────────────────────────────────────────

/// Stable opaque swap id assigned at `/v1/swap/initiate` time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SwapId(pub String);

impl SwapId {
    /// Generate a fresh id.
    pub fn fresh() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Parties + payloads (the immutable contents of `SwapState<P>`)
// ─────────────────────────────────────────────────────────────────────────────

/// Public-key handles for both parties. Bob is the webcash holder; Alice
/// is the ARK vtxo holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parties {
    /// Bob's PGP fingerprint — used as the addressing handle when the
    /// referee tells the push provider to deliver an `invalidate_hook`.
    pub bob_pgp_fp: PgpFingerprint,
    /// Bob's PGP pubkey (full key, hex) — Alice encrypts her partial-sig
    /// to this.
    pub bob_pgp_pubkey_hex: String,
    /// Alice's PGP fingerprint — used as the addressing handle when the
    /// referee tells the push provider to deliver an `insert_hook`.
    pub alice_pgp_fp: PgpFingerprint,
    /// Alice's PGP pubkey (full key, hex) — Bob encrypts the webcash
    /// secret to this.
    pub alice_pgp_pubkey_hex: String,
    /// Alice's MuSig2 share pubkey on secp256k1 (33-byte compressed,
    /// hex). The referee uses its own share + this to construct the
    /// 2-of-2 aggregated key.
    pub alice_musig2_pubkey: Secp256k1Pubkey,
    /// Bob's Ed25519 cancel pubkey (hex, 32 bytes). Authenticates
    /// `POST /v1/swap/{id}/cancel` from Bob's side. Independent of
    /// Bob's PGP key so a wallet can sign without unlocking PGP.
    pub bob_cancel_pubkey_hex: String,
    /// Alice's Ed25519 cancel pubkey (hex, 32 bytes). See
    /// [`Self::bob_cancel_pubkey_hex`].
    pub alice_cancel_pubkey_hex: String,
}

/// Bob's ZKP-verified ciphertext: the webcash secret encrypted to
/// Alice's PGP pubkey, with a Groth16 proof that decryption yields a
/// 32-byte value whose sha256 is `H_B`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BobPayload {
    /// Hex of `sha256(S_B)` — the public hash on webcash.org.
    pub h_b: WebcashPublicHash,
    /// Encrypted-to-Alice ciphertext.
    pub enc_secret_for_alice: PgpEncrypted<WebcashSecret>,
    /// Groth16 proof that ciphertext is honest.
    pub zkp_payload: Groth16Proof,
}

/// Alice's ZKP-verified ciphertext: her MuSig2 partial-sig on
/// `TX_settle`, encrypted to Bob's PGP pubkey, with a Groth16 proof that
/// decryption yields a valid MuSig2 partial-sig under her pubkey share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlicePayload {
    /// Hash of the ARK vtxo being swapped — locks the swap to a
    /// specific outpoint at audit time.
    pub vtxo: ArkOutpointHash,
    /// Hash of the canonical `TX_settle` bytes — what Alice's partial
    /// signs over.
    pub tx_settle_hash: String,
    /// Hash of the canonical `TX_refund` bytes — committed up front so
    /// post-hoc the audit can prove what tx Alice was refunding to.
    pub tx_refund_hash: String,
    /// Encrypted-to-Bob ciphertext (only Bob can decrypt; the referee
    /// forwards as opaque bytes).
    pub enc_partial_sig_for_bob: PgpEncrypted<AliceTxSettlePartialSig>,
    /// Groth16 proof that ciphertext is honest.
    pub zkp_signature: Groth16Proof,
}

/// Groth16 proof bytes + public inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Groth16Proof {
    /// Proof bytes (verifier consumes opaquely).
    pub proof: Vec<u8>,
    /// Public inputs in the order the verifier expects (per
    /// `docs/zkp-circuits.md`).
    pub public_inputs: Vec<Vec<u8>>,
}

/// MuSig2 nonce commitment Alice contributed at swap init. Two — one
/// per signing session (TX_settle and TX_refund). The referee's nonces
/// are kept in [`Musig2Sessions`] alongside.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliceMusig2Nonces {
    /// Hex of Alice's pub-nonce-commitment for TX_settle (66 bytes).
    pub settle_nonce_pub: String,
    /// Hex of Alice's pub-nonce-commitment for TX_refund (66 bytes).
    pub refund_nonce_pub: String,
}

/// The referee's MuSig2 sessions for a swap. The referee owns its
/// secret nonces; they never leave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Musig2Sessions {
    /// Hex of referee's pub-nonce for TX_settle (matches Alice's at
    /// swap-init time).
    pub settle_pub_nonce: String,
    /// Hex of referee's pub-nonce for TX_refund.
    pub refund_pub_nonce: String,
    /// Internal-only secret-nonce blob storage key. Real nonce material
    /// is in the secrets store keyed by `SwapId`; this field carries
    /// only a reference handle so we never accidentally serialise a
    /// secret nonce into the audit log.
    pub secret_nonce_handle: SwapId,
}

// ─────────────────────────────────────────────────────────────────────────────
// SwapState<Phase>
// ─────────────────────────────────────────────────────────────────────────────

use super::phases::Phase;
use std::marker::PhantomData as Pd;

/// The immutable per-phase value. Construction is private to
/// `transitions::*` so phases can only advance via documented transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapState<P: Phase> {
    /// Stable swap id.
    pub id: SwapId,
    /// Both parties' identities and pubkeys.
    pub parties: Parties,
    /// Bob's encrypted payload + ZKP.
    pub bob: BobPayload,
    /// Alice's encrypted payload + ZKP.
    pub alice: AlicePayload,
    /// Alice's nonce commitments (received at initiate).
    pub alice_nonces: AliceMusig2Nonces,
    /// Referee's MuSig2 sessions (created at initiate).
    pub referee_sessions: Musig2Sessions,
    /// Server-stamped Unix timestamp of when phase was entered.
    pub phase_entered_at: u64,
    /// Insert-push attempts so far (bounded by config).
    pub insert_push_attempts: u8,
    /// Hash of the audit-log entry that records this phase. Forms a
    /// tamper-evident chain when each entry commits to the prior tip.
    pub audit_tip_hex: String,
    /// PhantomData — the only thing that distinguishes phases at the type level.
    #[serde(skip)]
    pub(crate) _phase: Pd<P>,
}

impl<P: Phase> SwapState<P> {
    /// Stable phase tag for logs.
    pub const PHASE_NAME: &'static str = P::NAME;
}

/// The crate-internal "any phase" form used by the audit log + persistence
/// layer when a state's exact phase isn't known statically (e.g. when
/// loading from Postgres).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnyPhaseSwapState {
    /// Phase string (matches `Phase::NAME`).
    pub phase: String,
    /// Inner phase-erased payload.
    pub inner: serde_json::Value,
}

/// Map a phase name (matching `Phase::NAME`) to its canonical
/// signature tag. Single source of truth used by [`crate::audit`] when
/// signing audit-log entries — keeping it here avoids drift between the
/// state module and the audit module.
pub fn tag_for_phase(name: &str) -> Tag {
    match name {
        "init" => Tag::InitiateAck,
        "zkps-verified" => Tag::ZkpsVerified,
        "pre-checked" => Tag::PreChecked,
        "insert-pushed" => Tag::InsertPushed,
        "settled" => Tag::Settled,
        "aborted" => Tag::Aborted,
        "invalidated" => Tag::Invalidated,
        "refunded" => Tag::Refunded,
        "canceled" => Tag::Canceled,
        _ => Tag::AuditTip,
    }
}
