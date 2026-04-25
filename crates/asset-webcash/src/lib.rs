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

use std::collections::HashMap;

use webycash_asset_core::{
    Amount, Asset, AssetRecord, AssetSecret, AssetPublic, MintableAsset, RecordBuilder,
    RecordOrigin, Result as AssetResult, SplittableAsset,
};

/// In-DB record for a Webcash token. Mirrors the legacy
/// `webycash_server::db::TokenRecord` field-for-field so the new generic
/// storage layer can write the same Redis HASH shape and stay compatible
/// with deployed testnet data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebcashRecord {
    pub public_hash: String,
    pub amount_wats: i64,
    pub spent: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub spent_at: Option<chrono::DateTime<chrono::Utc>>,
    pub origin: WebcashOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebcashOrigin {
    Mined,
    Replaced,
}

impl AssetRecord for WebcashRecord {}

/// Webcash uses the historical Redis HASH field layout for backwards
/// compatibility with deployed testnet data.
impl webycash_storage::HashRecord for WebcashRecord {
    fn public_hash(&self) -> &str {
        &self.public_hash
    }

    fn amount_wats(&self) -> i64 {
        self.amount_wats
    }

    fn to_fields(&self, fields: &mut HashMap<String, String>) {
        fields.insert("amount_wats".into(), self.amount_wats.to_string());
        fields.insert("spent".into(), if self.spent { "1".into() } else { "0".into() });
        fields.insert("created_at".into(), self.created_at.to_rfc3339());
        if let Some(ts) = self.spent_at {
            fields.insert("spent_at".into(), ts.to_rfc3339());
        }
        fields.insert(
            "origin".into(),
            match self.origin {
                WebcashOrigin::Mined => "mined".into(),
                WebcashOrigin::Replaced => "replaced".into(),
            },
        );
    }

    fn from_fields(public_hash: &str, fields: &HashMap<String, String>) -> Option<Self> {
        Some(WebcashRecord {
            public_hash: public_hash.to_string(),
            amount_wats: fields.get("amount_wats")?.parse().ok()?,
            spent: fields.get("spent").map(|s| s == "1").unwrap_or(false),
            created_at: fields
                .get("created_at")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now),
            spent_at: fields
                .get("spent_at")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            origin: match fields.get("origin").map(|s| s.as_str()) {
                Some("replaced") => WebcashOrigin::Replaced,
                _ => WebcashOrigin::Mined,
            },
        })
    }
}

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

/// Bridge from a parsed `SecretWebcash` to a `WebcashRecord`. Used by the
/// /replace and /mining_report handlers to construct ledger entries.
impl RecordBuilder for Webcash {
    fn record_from_secret(secret: &SecretWebcash, origin: RecordOrigin) -> WebcashRecord {
        let public = secret.to_public();
        WebcashRecord {
            public_hash: public.hash,
            amount_wats: secret.amount.wats,
            spent: false,
            created_at: chrono::Utc::now(),
            spent_at: None,
            origin: match origin {
                RecordOrigin::Mined => WebcashOrigin::Mined,
                RecordOrigin::Replaced => WebcashOrigin::Replaced,
            },
        }
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
