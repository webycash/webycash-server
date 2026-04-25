//! Asset implementations for RGB contracts.
//!
//! Two distinct asset types share this crate:
//! - `RgbFungible` (RGB20-like, splittable, fungible): implements
//!   `Asset + SplittableAsset + IssuedAsset + MintableAsset + RecordBuilder`.
//! - `RgbCollectible` (RGB21-like, non-splittable, NFT): implements
//!   `Asset + TransferableAsset + IssuedAsset + MintableAsset`.
//!
//! Compile-time gating means a server binary built for `RgbCollectible`
//! cannot serve `/api/v1/replace` (no `SplittableAsset` impl).
//!
//! Wire format (per the approved 2026-04-25 plan):
//!   - Fungible:    `e{amount}:secret:{hex64}:{contract_id}:{issuer_fp}`
//!   - Collectible: `secret:{hex64}:{contract_id}:{issuer_fp}`
//!
//! AluVM execution + rgb-core/rgb-std integration are scoped in M3
//! follow-ups and live in `webycash-aluvm-runtime` once the WASM viability
//! gate completes.

#![forbid(unsafe_code)]

mod token;

pub use token::{
    PublicCollectible, PublicFungible, SecretCollectible, SecretFungible, TokenError,
};

use std::collections::HashMap;

use webycash_asset_core::{
    Amount, Asset, AssetPublic, AssetRecord, AssetSecret, CollectibleRecordBuilder, ContractId,
    IssuedAsset, MintableAsset, PgpFingerprint, RecordBuilder, RecordOrigin,
    Result as AssetResult, SplittableAsset, TransferableAsset,
};

// ─────────────────────────────────────────────────────────────────────────────
// Origin tags + records
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RgbOrigin {
    Mined,
    Issued,
    Replaced,
    Transferred,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RgbFungibleRecord {
    pub public_hash: String,
    pub amount_wats: i64,
    pub spent: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub spent_at: Option<chrono::DateTime<chrono::Utc>>,
    pub origin: RgbOrigin,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

impl AssetRecord for RgbFungibleRecord {}

impl webycash_storage::HashRecord for RgbFungibleRecord {
    fn public_hash(&self) -> &str {
        &self.public_hash
    }
    fn amount_wats(&self) -> i64 {
        self.amount_wats
    }
    fn namespace(&self) -> webycash_storage::Namespace {
        webycash_storage::Namespace::scoped(self.contract_id.clone(), self.issuer_fp.clone())
    }
    fn to_fields(&self, fields: &mut HashMap<String, String>) {
        fields.insert("amount_wats".into(), self.amount_wats.to_string());
        fields.insert(
            "spent".into(),
            if self.spent { "1".into() } else { "0".into() },
        );
        fields.insert("created_at".into(), self.created_at.to_rfc3339());
        if let Some(ts) = self.spent_at {
            fields.insert("spent_at".into(), ts.to_rfc3339());
        }
        fields.insert(
            "origin".into(),
            match self.origin {
                RgbOrigin::Mined => "mined",
                RgbOrigin::Issued => "issued",
                RgbOrigin::Replaced => "replaced",
                RgbOrigin::Transferred => "transferred",
            }
            .into(),
        );
        fields.insert("contract_id".into(), self.contract_id.0.clone());
        fields.insert("issuer_fp".into(), self.issuer_fp.0.clone());
    }
    fn from_fields(public_hash: &str, fields: &HashMap<String, String>) -> Option<Self> {
        Some(RgbFungibleRecord {
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
                Some("issued") => RgbOrigin::Issued,
                Some("replaced") => RgbOrigin::Replaced,
                Some("transferred") => RgbOrigin::Transferred,
                _ => RgbOrigin::Mined,
            },
            contract_id: ContractId(fields.get("contract_id")?.clone()),
            issuer_fp: PgpFingerprint(fields.get("issuer_fp")?.clone()),
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RgbCollectibleRecord {
    pub public_hash: String,
    pub spent: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub spent_at: Option<chrono::DateTime<chrono::Utc>>,
    pub origin: RgbOrigin,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

impl AssetRecord for RgbCollectibleRecord {}

impl webycash_storage::HashRecord for RgbCollectibleRecord {
    fn public_hash(&self) -> &str {
        &self.public_hash
    }
    fn amount_wats(&self) -> i64 {
        // NFTs have no fungible amount; report 0.
        0
    }
    fn namespace(&self) -> webycash_storage::Namespace {
        webycash_storage::Namespace::scoped(self.contract_id.clone(), self.issuer_fp.clone())
    }
    fn to_fields(&self, fields: &mut HashMap<String, String>) {
        // amount_wats=0 marks the record as collectible (no amount slot in
        // the wire form). Storing it keeps the HASH layout uniform with
        // fungible records so a single Lua script can handle both.
        fields.insert("amount_wats".into(), "0".into());
        fields.insert(
            "spent".into(),
            if self.spent { "1".into() } else { "0".into() },
        );
        fields.insert("created_at".into(), self.created_at.to_rfc3339());
        if let Some(ts) = self.spent_at {
            fields.insert("spent_at".into(), ts.to_rfc3339());
        }
        fields.insert(
            "origin".into(),
            match self.origin {
                RgbOrigin::Mined => "mined",
                RgbOrigin::Issued => "issued",
                RgbOrigin::Replaced => "replaced",
                RgbOrigin::Transferred => "transferred",
            }
            .into(),
        );
        fields.insert("contract_id".into(), self.contract_id.0.clone());
        fields.insert("issuer_fp".into(), self.issuer_fp.0.clone());
    }
    fn from_fields(public_hash: &str, fields: &HashMap<String, String>) -> Option<Self> {
        Some(RgbCollectibleRecord {
            public_hash: public_hash.to_string(),
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
                Some("issued") => RgbOrigin::Issued,
                Some("replaced") => RgbOrigin::Replaced,
                Some("transferred") => RgbOrigin::Transferred,
                _ => RgbOrigin::Mined,
            },
            contract_id: ContractId(fields.get("contract_id")?.clone()),
            issuer_fp: PgpFingerprint(fields.get("issuer_fp")?.clone()),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Issuance contexts (operator-signed mint envelope, AluVM stub)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RgbFungibleIssuance {
    pub records: Vec<RgbFungibleRecord>,
}

#[derive(Debug, Clone)]
pub struct RgbCollectibleIssuance {
    pub records: Vec<RgbCollectibleRecord>,
}

// ─────────────────────────────────────────────────────────────────────────────
// RGB20 fungible asset
// ─────────────────────────────────────────────────────────────────────────────

pub struct RgbFungible;

impl Asset for RgbFungible {
    const NAME: &'static str = "rgb-fungible";
    type Secret = SecretFungible;
    type Public = PublicFungible;
    type Record = RgbFungibleRecord;

    fn parse_secret(s: &str) -> AssetResult<Self::Secret> {
        SecretFungible::parse(s)
            .map_err(|e| webycash_asset_core::AssetError::Parse(format!("rgb20 secret: {e}")))
    }
    fn parse_public(s: &str) -> AssetResult<Self::Public> {
        PublicFungible::parse(s)
            .map_err(|e| webycash_asset_core::AssetError::Parse(format!("rgb20 public: {e}")))
    }
    fn to_public(secret: &Self::Secret) -> Self::Public {
        secret.to_public()
    }
}

impl SplittableAsset for RgbFungible {
    fn amount(secret: &Self::Secret) -> Amount {
        secret.amount
    }
    fn amount_public(public: &Self::Public) -> Amount {
        public.amount
    }
}

impl IssuedAsset for RgbFungible {
    fn issuer(secret: &Self::Secret) -> &PgpFingerprint {
        &secret.issuer_fp
    }
    fn issuer_public(public: &Self::Public) -> &PgpFingerprint {
        &public.issuer_fp
    }
    fn contract_id(secret: &Self::Secret) -> &ContractId {
        &secret.contract_id
    }
    fn contract_id_public(public: &Self::Public) -> &ContractId {
        &public.contract_id
    }
}

impl MintableAsset for RgbFungible {
    type IssuanceContext = RgbFungibleIssuance;
    fn verify_issuance(_ctx: &Self::IssuanceContext) -> AssetResult<()> {
        // AluVM transition validation lives in webycash-aluvm-runtime (M3
        // follow-up). Issuer signature check happens in webycash-auth.
        Ok(())
    }
    fn build_records(ctx: &Self::IssuanceContext) -> AssetResult<Vec<Self::Record>> {
        Ok(ctx.records.clone())
    }
}

impl RecordBuilder for RgbFungible {
    fn record_from_secret(secret: &Self::Secret, origin: RecordOrigin) -> Self::Record {
        let public = secret.to_public();
        RgbFungibleRecord {
            public_hash: public.hash,
            amount_wats: secret.amount.wats,
            spent: false,
            created_at: chrono::Utc::now(),
            spent_at: None,
            origin: match origin {
                RecordOrigin::Mined => RgbOrigin::Mined,
                RecordOrigin::Replaced => RgbOrigin::Replaced,
            },
            contract_id: secret.contract_id.clone(),
            issuer_fp: secret.issuer_fp.clone(),
        }
    }

    fn namespace_envelope(secret: &Self::Secret) -> Option<(String, String)> {
        Some((secret.contract_id.0.clone(), secret.issuer_fp.0.clone()))
    }

    fn public_namespace_envelope(public: &Self::Public) -> Option<(String, String)> {
        Some((public.contract_id.0.clone(), public.issuer_fp.0.clone()))
    }
}

impl AssetSecret for SecretFungible {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn secret_hex(&self) -> &str {
        &self.secret
    }
}
impl AssetPublic for PublicFungible {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn public_hash(&self) -> &str {
        &self.hash
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RGB21 collectible asset (non-splittable, transfer-only)
// ─────────────────────────────────────────────────────────────────────────────

pub struct RgbCollectible;

impl Asset for RgbCollectible {
    const NAME: &'static str = "rgb-collectible";
    type Secret = SecretCollectible;
    type Public = PublicCollectible;
    type Record = RgbCollectibleRecord;

    fn parse_secret(s: &str) -> AssetResult<Self::Secret> {
        SecretCollectible::parse(s)
            .map_err(|e| webycash_asset_core::AssetError::Parse(format!("rgb21 secret: {e}")))
    }
    fn parse_public(s: &str) -> AssetResult<Self::Public> {
        PublicCollectible::parse(s)
            .map_err(|e| webycash_asset_core::AssetError::Parse(format!("rgb21 public: {e}")))
    }
    fn to_public(secret: &Self::Secret) -> Self::Public {
        secret.to_public()
    }
}

// RGB21 implements TransferableAsset, NOT SplittableAsset.
impl TransferableAsset for RgbCollectible {
    fn validate_transfer(
        _input: &Self::Secret,
        _output: &Self::Secret,
    ) -> webycash_asset_core::Result<()> {
        // AluVM transition + namespace check happen here; stubbed for M3
        // follow-up. The /transfer endpoint enforces (contract_id, issuer_fp)
        // namespace match server-side.
        Ok(())
    }
}

impl IssuedAsset for RgbCollectible {
    fn issuer(secret: &Self::Secret) -> &PgpFingerprint {
        &secret.issuer_fp
    }
    fn issuer_public(public: &Self::Public) -> &PgpFingerprint {
        &public.issuer_fp
    }
    fn contract_id(secret: &Self::Secret) -> &ContractId {
        &secret.contract_id
    }
    fn contract_id_public(public: &Self::Public) -> &ContractId {
        &public.contract_id
    }
}

impl MintableAsset for RgbCollectible {
    type IssuanceContext = RgbCollectibleIssuance;
    fn verify_issuance(_ctx: &Self::IssuanceContext) -> AssetResult<()> {
        Ok(())
    }
    fn build_records(ctx: &Self::IssuanceContext) -> AssetResult<Vec<Self::Record>> {
        Ok(ctx.records.clone())
    }
}

impl CollectibleRecordBuilder for RgbCollectible {
    fn record_from_secret(secret: &Self::Secret, origin: RecordOrigin) -> Self::Record {
        let public = secret.to_public();
        RgbCollectibleRecord {
            public_hash: public.hash,
            spent: false,
            created_at: chrono::Utc::now(),
            spent_at: None,
            origin: match origin {
                RecordOrigin::Mined => RgbOrigin::Mined,
                RecordOrigin::Replaced => RgbOrigin::Transferred,
            },
            contract_id: secret.contract_id.clone(),
            issuer_fp: secret.issuer_fp.clone(),
        }
    }

    fn namespace_envelope(secret: &Self::Secret) -> Option<(String, String)> {
        Some((secret.contract_id.0.clone(), secret.issuer_fp.0.clone()))
    }

    fn public_namespace_envelope(public: &Self::Public) -> Option<(String, String)> {
        Some((public.contract_id.0.clone(), public.issuer_fp.0.clone()))
    }
}

impl AssetSecret for SecretCollectible {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn secret_hex(&self) -> &str {
        &self.secret
    }
}
impl AssetPublic for PublicCollectible {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn public_hash(&self) -> &str {
        &self.hash
    }
}
