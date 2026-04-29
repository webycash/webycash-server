//! Core trait hierarchy for the asset-gated webycash server family.
//!
//! Five traits compose at compile time to gate which endpoints exist on a
//! given server flavor binary:
//!
//! Server endpoint surface (every flavor uses the SAME endpoints; the
//! /api/v1/replace handler enforces conservation for splittable
//! assets and 1:1 arity for transferable ones):
//!
//! | Endpoint                       | Required trait bound                           |
//! |--------------------------------|------------------------------------------------|
//! | `/api/v1/health_check`         | `Asset`                                        |
//! | `/api/v1/burn`                 | `Asset`                                        |
//! | `/api/v1/replace`              | `SplittableAsset` OR `TransferableAsset`       |
//! | `/api/v1/mining_report`        | `MintableAsset` (mining-only — RGB21 absent)   |
//! | `/api/v1/issue`                | `IssuedAsset + MintableAsset`                  |
//! | `/api/v1/issuer/{fp}/stats`    | `IssuedAsset`                                  |

#![forbid(unsafe_code)]
#![warn(missing_docs)]

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
    /// Parse a 40-char lowercase hex fingerprint (20-byte OpenPGP V4
    /// shape). Rejects wrong length, uppercase, and non-hex digits.
    /// Cryptographic validity (cert binding, key algorithm) lives in
    /// `webycash-auth` — this is shape-only.
    ///
    /// ```
    /// use webycash_asset_core::PgpFingerprint;
    /// let fp = PgpFingerprint::parse("aabbccddeeff00112233445566778899aabbccdd").unwrap();
    /// assert_eq!(fp.0.len(), 40);
    ///
    /// // Rejects uppercase, wrong length, non-hex.
    /// assert!(PgpFingerprint::parse("AABBCCDDEEFF00112233445566778899AABBCCDD").is_err());
    /// assert!(PgpFingerprint::parse("aabb").is_err());
    /// assert!(PgpFingerprint::parse(&"z".repeat(40)).is_err());
    /// ```
    pub fn parse(s: &str) -> Result<Self> {
        if s.len() != 40 {
            return Err(AssetError::Parse(format!(
                "PgpFingerprint must be 40 hex chars, got {}",
                s.len()
            )));
        }
        if !s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return Err(AssetError::Parse(
                "PgpFingerprint must be lowercase hex".into(),
            ));
        }
        Ok(PgpFingerprint(s.to_string()))
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

impl ContractId {
    /// Parse a ContractId. Accepts the wire-format slug shape that
    /// every flavor's nom parser also accepts:
    ///   - 1..=64 chars
    ///   - each char is alphanumeric, `-`, or `_`
    ///
    /// Bech32m strings (the RGB shape) and issuer-chosen series ids
    /// (the Voucher shape) both fit inside this superset.
    ///
    /// ```
    /// use webycash_asset_core::ContractId;
    /// assert!(ContractId::parse("rgb20-usdc").is_ok());
    /// assert!(ContractId::parse("credits-2026-q1").is_ok());
    /// // empty / over-64 / disallowed punctuation reject
    /// assert!(ContractId::parse("").is_err());
    /// assert!(ContractId::parse(&"a".repeat(65)).is_err());
    /// assert!(ContractId::parse("rgb20:usdc").is_err());
    /// ```
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(AssetError::Parse("ContractId cannot be empty".into()));
        }
        if s.len() > 64 {
            return Err(AssetError::Parse(format!(
                "ContractId longer than 64 chars: {}",
                s.len()
            )));
        }
        if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(AssetError::Parse(
                "ContractId must be alphanumeric, '-', or '_'".into(),
            ));
        }
        Ok(ContractId(s.to_string()))
    }
}

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// All errors the asset-core trait surface can produce.
#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    /// Wire-format token failed to parse (invalid prefix, bad hex,
    /// trailing input, etc.).
    #[error("parse error: {0}")]
    Parse(String),
    /// A type-level invariant was violated (e.g. cross-namespace
    /// inputs to a single replace).
    #[error("invariant violated: {0}")]
    Invariant(String),
    /// Underlying `Amount` arithmetic failure (overflow / parse).
    #[error("amount error: {0}")]
    Amount(#[from] AmountError),
    /// Trait method that hasn't been wired yet for this asset.
    #[error("unimplemented: {0}")]
    Unimplemented(&'static str),
}

/// Convenience alias used across the asset-core trait surface.
pub type Result<T> = std::result::Result<T, AssetError>;

// ---------------------------------------------------------------------------
// Trait surface
// ---------------------------------------------------------------------------

/// Marker for a parsed `:secret:` token. The asset crate provides the concrete
/// type implementing this, including amount/issuer/contract accessors as
/// applicable.
pub trait AssetSecret: Send + Sync + fmt::Debug + Clone + 'static {
    /// Render the secret as its canonical wire-format string.
    fn wire_form(&self) -> String;
    /// The bare hex secret value (without prefix/amount/issuer/contract).
    fn secret_hex(&self) -> &str;
}

/// Marker for a parsed `:public:` token.
pub trait AssetPublic: Send + Sync + fmt::Debug + Clone + 'static {
    /// Render the public token as its canonical wire-format string.
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

    /// Concrete `:secret:` token type for this asset.
    type Secret: AssetSecret;
    /// Concrete `:public:` token type for this asset.
    type Public: AssetPublic;
    /// Concrete in-DB record type for this asset.
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
/// `/api/v1/replace` on `IssuedAsset` types requires all inputs and all
/// outputs to share the same `(contract_id, issuer)` pair (server enforces
/// atomically; client-side wallets can pre-flight the same check).
pub trait IssuedAsset: Asset {
    /// Issuer fingerprint borrowed from a parsed secret.
    fn issuer(secret: &Self::Secret) -> &PgpFingerprint;
    /// Issuer fingerprint borrowed from a parsed public token.
    fn issuer_public(public: &Self::Public) -> &PgpFingerprint;
    /// Contract id borrowed from a parsed secret.
    fn contract_id(secret: &Self::Secret) -> &ContractId;
    /// Contract id borrowed from a parsed public token.
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

/// How a record entered the ledger. Used by `RecordBuilder` to tag inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordOrigin {
    /// Token was minted via PoW (mining_report endpoint).
    Mined,
    /// Token was created by splitting/replacing existing tokens.
    Replaced,
}

impl fmt::Display for RecordOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordOrigin::Mined => f.write_str("mined"),
            RecordOrigin::Replaced => f.write_str("replaced"),
        }
    }
}

/// Bridge from a parsed secret to the asset's storage record. Used by the
/// `/replace` and `/mining_report` handlers in `server-core` to construct
/// ledger entries without server-core needing to know each asset's record
/// shape.
///
/// `namespace_envelope` / `public_namespace_envelope` let `IssuedAsset`
/// flavors (RGB, Voucher) report their `(contract_id, issuer_fp)`
/// partition. Webcash uses the default (unscoped) implementation.
pub trait RecordBuilder: SplittableAsset {
    /// Build the storage record corresponding to a parsed secret,
    /// tagged with how the record entered the ledger.
    fn record_from_secret(secret: &Self::Secret, origin: RecordOrigin) -> Self::Record;

    /// Returns `(contract_id, issuer_fp)` for a secret, if the asset is
    /// issuer-namespaced. Default: `None` (Webcash).
    fn namespace_envelope(_secret: &Self::Secret) -> Option<(String, String)> {
        None
    }

    /// Returns `(contract_id, issuer_fp)` for a public token, if the asset
    /// is issuer-namespaced. Default: `None`.
    fn public_namespace_envelope(_public: &Self::Public) -> Option<(String, String)> {
        None
    }
}

/// Analog of `RecordBuilder` for non-splittable / collectible (NFT) assets.
/// `RgbCollectible` implements this; the collectible `/api/v1/replace`
/// handler (1:1 arity) uses it instead of the splittable variant.
pub trait CollectibleRecordBuilder: TransferableAsset {
    /// Build the storage record corresponding to a parsed secret,
    /// tagged with how the record entered the ledger.
    fn record_from_secret(secret: &Self::Secret, origin: RecordOrigin) -> Self::Record;

    /// Returns `(contract_id, issuer_fp)` for a secret. RGB21 always has
    /// both; default returns `None` for parity with `RecordBuilder`.
    fn namespace_envelope(_secret: &Self::Secret) -> Option<(String, String)> {
        None
    }

    /// Same as `namespace_envelope` but for the public-token form.
    fn public_namespace_envelope(_public: &Self::Public) -> Option<(String, String)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgp_fingerprint_accepts_canonical_form() {
        let fp = PgpFingerprint::parse("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        assert_eq!(fp.0, "aabbccddeeff00112233445566778899aabbccdd");
        assert_eq!(fp.to_string(), "aabbccddeeff00112233445566778899aabbccdd");
    }

    #[test]
    fn pgp_fingerprint_rejects_uppercase() {
        let err = PgpFingerprint::parse("AABBCCDDEEFF00112233445566778899AABBCCDD").unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn pgp_fingerprint_rejects_short_input() {
        let err = PgpFingerprint::parse("aabb").unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn pgp_fingerprint_rejects_long_input() {
        let err = PgpFingerprint::parse(&"a".repeat(41)).unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn pgp_fingerprint_rejects_non_hex() {
        let err = PgpFingerprint::parse(&"z".repeat(40)).unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn pgp_fingerprint_rejects_mixed_case() {
        let err = PgpFingerprint::parse("Aabbccddeeff00112233445566778899aabbccdd")
            .unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn contract_id_accepts_alphanumeric_dash_underscore() {
        assert!(ContractId::parse("rgb20").is_ok());
        assert!(ContractId::parse("rgb20-usdc").is_ok());
        assert!(ContractId::parse("credits_2026_q1").is_ok());
        assert!(ContractId::parse(&"a".repeat(64)).is_ok());
        assert!(ContractId::parse("a").is_ok());
    }

    #[test]
    fn contract_id_rejects_empty() {
        let err = ContractId::parse("").unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn contract_id_rejects_too_long() {
        let err = ContractId::parse(&"a".repeat(65)).unwrap_err();
        assert!(matches!(err, AssetError::Parse(_)));
    }

    #[test]
    fn contract_id_rejects_punctuation() {
        for bad in [":", ".", "/", " ", "rgb:usdc", "rgb.usdc", "rgb usdc"] {
            let err = ContractId::parse(bad).unwrap_err();
            assert!(matches!(err, AssetError::Parse(_)), "{bad:?} should reject");
        }
    }

    #[test]
    fn record_origin_displays_lowercase() {
        assert_eq!(RecordOrigin::Mined.to_string(), "mined");
        assert_eq!(RecordOrigin::Replaced.to_string(), "replaced");
    }
}
