//! Core trait hierarchy for the asset-gated webycash server family.
//!
//! Five traits compose at compile time to gate which endpoints exist on a
//! given server flavor binary:
//!
//! | Endpoint                       | Required trait bound                           |
//! |--------------------------------|------------------------------------------------|
//! | `/api/v1/health_check`         | `Asset`                                        |
//! | `/api/v1/burn`                 | `Asset`                                        |
//! | `/api/v1/replace`              | `SplittableAsset` (RGB21 NFT excluded)         |
//! | `/api/v1/transfer`             | `TransferableAsset`                            |
//! | `/api/v1/mining_report`        | `MintableAsset`                                |
//! | `/api/v1/issue`                | `IssuedAsset + MintableAsset`                  |
//! | `/api/v1/issuer/{fp}/stats`    | `IssuedAsset`                                  |

#![forbid(unsafe_code)]

pub mod amount;

pub use amount::{Amount, AmountError};

use serde::{Deserialize, Serialize};
use std::fmt;

/// Hex-encoded OpenPGP V4 fingerprint (20 bytes / 40 lowercase hex chars) of
/// an issuer's Ed25519 cert. Used in `IssuedAsset` wire formats and as part
/// of the storage partition key for RGB and Voucher flavors.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PgpFingerprint(pub String);

impl PgpFingerprint {
    /// Lowercase hex bytes, no spaces. Returns an error for non-hex / wrong-length input.
    /// Real validation lands in `webycash-auth` (M3).
    pub fn parse(_s: &str) -> Result<Self> {
        Err(AssetError::Unimplemented(
            "PgpFingerprint::parse — lands in M3",
        ))
    }
}

impl fmt::Display for PgpFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical contract identifier within an asset flavor.
///
/// - For RGB: `rgb_std::ContractId` stringified as Bech32m-without-checksum.
/// - For Voucher: an issuer-chosen UTF-8 series identifier (alphanumeric + `-` / `_`, max 64 chars).
/// - For Webcash: this slot does not exist on the wire; storage uses a constant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContractId(pub String);

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// All errors the asset-core trait surface can produce.
#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("invariant violated: {0}")]
    Invariant(String),
    #[error("amount error: {0}")]
    Amount(#[from] AmountError),
    #[error("unimplemented: {0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, AssetError>;

// ---------------------------------------------------------------------------
// Trait surface
// ---------------------------------------------------------------------------

/// Marker for a parsed `:secret:` token. The asset crate provides the concrete
/// type implementing this, including amount/issuer/contract accessors as
/// applicable.
pub trait AssetSecret: Send + Sync + fmt::Debug + Clone + 'static {
    fn wire_form(&self) -> String;
    /// The bare hex secret value (without prefix/amount/issuer/contract).
    fn secret_hex(&self) -> &str;
}

/// Marker for a parsed `:public:` token.
pub trait AssetPublic: Send + Sync + fmt::Debug + Clone + 'static {
    fn wire_form(&self) -> String;
    /// The hex SHA256 hash that uniquely identifies the token within its namespace.
    fn public_hash(&self) -> &str;
}

/// In-database token record. Storage backends serialize/deserialize this via
/// the strict-types schema (RGB) or serde_json (Webcash/Voucher).
pub trait AssetRecord: Send + Sync + fmt::Debug + Clone + 'static {}

/// Base trait every asset implements. No implication of fungibility,
/// splittability, or issuer scoping — those are layered on by extension
/// traits.
pub trait Asset: Send + Sync + 'static {
    /// Lower-case ASCII name: `"webcash"`, `"rgb"`, `"voucher"`.
    /// Used in storage keys and config sections.
    const NAME: &'static str;

    type Secret: AssetSecret;
    type Public: AssetPublic;
    type Record: AssetRecord;

    /// Parse a `:secret:` token from its canonical wire form.
    fn parse_secret(s: &str) -> Result<Self::Secret>;

    /// Parse a `:public:` token from its canonical wire form.
    fn parse_public(s: &str) -> Result<Self::Public>;

    /// Derive the public token from a secret. For Webcash:
    /// `sha256(secret_hex_bytes)` — frozen by the conformance suite.
    fn to_public(secret: &Self::Secret) -> Self::Public;
}

/// Assets whose units can be split or merged: Webcash, RGB20, ALL Vouchers.
/// RGB21 NFTs do NOT implement this — `/api/v1/replace` is statically
/// unavailable on the RGB21 binary.
pub trait SplittableAsset: Asset {
    /// Atomic-unit amount carried by the secret.
    fn amount(secret: &Self::Secret) -> Amount;

    /// Atomic-unit amount carried by the public token.
    fn amount_public(public: &Self::Public) -> Amount;
}

/// Assets that move 1:1 between owners (no split): RGB21 NFTs.
pub trait TransferableAsset: Asset {
    /// Validate a transfer (asset-specific, e.g., AluVM transition for RGB21).
    fn validate_transfer(input: &Self::Secret, output: &Self::Secret) -> Result<()>;
}

/// Assets whose secrets carry an issuer's PGP fingerprint and an issuer-scoped
/// contract identifier. Webcash does NOT implement this; RGB and Voucher do.
///
/// Replace and transfer operations on `IssuedAsset` types require all inputs
/// and all outputs to share the same `(contract_id, issuer)` pair.
pub trait IssuedAsset: Asset {
    fn issuer(secret: &Self::Secret) -> &PgpFingerprint;
    fn issuer_public(public: &Self::Public) -> &PgpFingerprint;
    fn contract_id(secret: &Self::Secret) -> &ContractId;
    fn contract_id_public(public: &Self::Public) -> &ContractId;
}

/// Configuration for the mining/issuance gate.
///
/// Concrete fields (mode, target_secs, etc.) live in `webycash-mining`;
/// this trait only references them via an opaque type so asset-core stays
/// dependency-free.
pub trait MintableAsset: Asset {
    /// Asset-specific issuance request payload (PoW preimage for Webcash;
    /// signed mint envelope for RGB/Voucher).
    type IssuanceContext: Send + Sync + 'static;

    /// Verify the issuance request meets the configured mining/auth policy.
    fn verify_issuance(ctx: &Self::IssuanceContext) -> Result<()>;

    /// Build the records to insert into the ledger upon a successful issuance.
    fn build_records(ctx: &Self::IssuanceContext) -> Result<Vec<Self::Record>>;
}
