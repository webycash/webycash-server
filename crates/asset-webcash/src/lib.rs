//! Asset implementation for Webcash.
//!
//! WIRE-PROTOCOL FROZEN. This crate must remain bit-for-bit compatible with
//! `https://webcash.org` production. The `webycash-conformance` crate gates
//! every change against captured production fixtures and a live-host smoke
//! test.
//!
//! Implements the `Asset` + `SplittableAsset` + `MintableAsset` traits from
//! `webycash-asset-core`. Webcash does NOT implement `IssuedAsset` (it has
//! no issuer-namespacing) or `TransferableAsset` (units are always
//! splittable).

#![forbid(unsafe_code)]

mod token;

pub use token::{PublicWebcash, SecretWebcash, TokenError};

use webycash_asset_core::{
    Amount, Asset, AssetRecord, AssetSecret, AssetPublic, MintableAsset, Result as AssetResult,
    SplittableAsset,
};

/// In-DB record for a Webcash token. Mirrors the legacy
/// `webycash_server::db::TokenRecord`. Migrated in M1.
#[derive(Debug, Clone)]
pub struct WebcashRecord {
    pub public_hash: String,
    pub amount_wats: i64,
    pub spent: bool,
    pub origin: WebcashOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebcashOrigin {
    Mined,
    Replaced,
}

impl AssetRecord for WebcashRecord {}

/// Issuance context for Webcash: the PoW preimage submitted to
/// `/api/v1/mining_report`. Verified against the current difficulty target.
#[derive(Debug, Clone)]
pub struct WebcashMiningReport {
    pub preimage: String,
    pub difficulty_target_bits: u32,
}

/// Zero-sized type identifying the Webcash asset flavor.
pub struct Webcash;

impl Asset for Webcash {
    const NAME: &'static str = "webcash";
    type Secret = SecretWebcash;
    type Public = PublicWebcash;
    type Record = WebcashRecord;

    fn parse_secret(s: &str) -> AssetResult<Self::Secret> {
        SecretWebcash::parse(s).map_err(|e| {
            webycash_asset_core::AssetError::Parse(format!("webcash secret: {e}"))
        })
    }

    fn parse_public(s: &str) -> AssetResult<Self::Public> {
        PublicWebcash::parse(s).map_err(|e| {
            webycash_asset_core::AssetError::Parse(format!("webcash public: {e}"))
        })
    }

    fn to_public(secret: &Self::Secret) -> Self::Public {
        secret.to_public()
    }
}

impl SplittableAsset for Webcash {
    fn amount(secret: &Self::Secret) -> Amount {
        secret.amount
    }
    fn amount_public(public: &Self::Public) -> Amount {
        public.amount
    }
}

impl MintableAsset for Webcash {
    type IssuanceContext = WebcashMiningReport;

    fn verify_issuance(_ctx: &Self::IssuanceContext) -> AssetResult<()> {
        // Real PoW verification migrated in M1.D from
        // crates/server/src/protocol/mining.rs::verify_pow.
        Err(webycash_asset_core::AssetError::Unimplemented(
            "MintableAsset::verify_issuance for Webcash — wired in M1.D",
        ))
    }

    fn build_records(_ctx: &Self::IssuanceContext) -> AssetResult<Vec<Self::Record>> {
        Err(webycash_asset_core::AssetError::Unimplemented(
            "MintableAsset::build_records for Webcash — wired in M1.D",
        ))
    }
}

// AssetSecret/AssetPublic trait impls live alongside SecretWebcash/PublicWebcash
// in the `token` module so all Webcash wire concerns are colocated.
impl AssetSecret for SecretWebcash {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn secret_hex(&self) -> &str {
        &self.secret
    }
}

impl AssetPublic for PublicWebcash {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn public_hash(&self) -> &str {
        &self.hash
    }
}
