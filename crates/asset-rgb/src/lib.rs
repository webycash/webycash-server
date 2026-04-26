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
}

impl std::fmt::Display for RgbOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RgbOrigin::Mined => f.write_str("mined"),
            RgbOrigin::Issued => f.write_str("issued"),
            RgbOrigin::Replaced => f.write_str("replaced"),
        }
    }
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
                // "transferred" is a legacy spelling from when the
                // collectible flavor exposed /api/v1/transfer; we
                // unified to /api/v1/replace and now emit "replaced",
                // but old DB rows still need to load.
                Some("replaced") | Some("transferred") => RgbOrigin::Replaced,
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
                // "transferred" is a legacy spelling from when the
                // collectible flavor exposed /api/v1/transfer; we
                // unified to /api/v1/replace and now emit "replaced",
                // but old DB rows still need to load.
                Some("replaced") | Some("transferred") => RgbOrigin::Replaced,
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

    /// Shape-only check: every record in the batch must share the same
    /// `(contract_id, issuer_fp)` — issuance is single-namespace by
    /// construction, and a mixed batch would corrupt the storage
    /// partition. AluVM transition validation lives in
    /// webycash-aluvm-runtime (M3 follow-up); issuer signature
    /// verification happens in webycash-auth.
    fn verify_issuance(ctx: &Self::IssuanceContext) -> AssetResult<()> {
        let mut iter = ctx.records.iter();
        let Some(first) = iter.next() else {
            return Ok(()); // empty issuance is degenerate but not invalid
        };
        for r in iter {
            if r.contract_id != first.contract_id {
                return Err(webycash_asset_core::AssetError::Invariant(format!(
                    "issuance batch crosses contract_id: {} vs {}",
                    first.contract_id, r.contract_id,
                )));
            }
            if r.issuer_fp != first.issuer_fp {
                return Err(webycash_asset_core::AssetError::Invariant(format!(
                    "issuance batch crosses issuer_fp: {} vs {}",
                    first.issuer_fp, r.issuer_fp,
                )));
            }
        }
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
    /// Pre-flight check before submitting an RGB21 1:1 transfer. Pins
    /// the namespace invariant — input and output must carry the same
    /// `(contract_id, issuer_fp)` pair — locally, so a wallet bug
    /// surfaces with a clear error rather than as a server 422.
    ///
    /// Real AluVM transition validation happens in the wallet (see
    /// `webylib-wasm` for the browser path / `webylib-aluvm` for
    /// native); this layer is shape-only.
    fn validate_transfer(
        input: &Self::Secret,
        output: &Self::Secret,
    ) -> webycash_asset_core::Result<()> {
        if input.contract_id != output.contract_id {
            return Err(webycash_asset_core::AssetError::Invariant(format!(
                "contract_id mismatch: {} vs {}",
                input.contract_id, output.contract_id,
            )));
        }
        if input.issuer_fp != output.issuer_fp {
            return Err(webycash_asset_core::AssetError::Invariant(format!(
                "issuer_fp mismatch: {} vs {}",
                input.issuer_fp, output.issuer_fp,
            )));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: u8) -> PgpFingerprint {
        PgpFingerprint(format!("{byte:02x}").repeat(20))
    }

    fn collectible(secret: &str, contract: &str, issuer: PgpFingerprint) -> SecretCollectible {
        SecretCollectible {
            secret: secret.to_string(),
            contract_id: ContractId(contract.to_string()),
            issuer_fp: issuer,
        }
    }

    #[test]
    fn validate_transfer_accepts_same_namespace() {
        let issuer = fp(0xaa);
        let a = collectible(&"a".repeat(64), "rgb21-art", issuer.clone());
        let b = collectible(&"b".repeat(64), "rgb21-art", issuer);
        assert!(RgbCollectible::validate_transfer(&a, &b).is_ok());
    }

    #[test]
    fn validate_transfer_rejects_contract_mismatch() {
        let issuer = fp(0xaa);
        let a = collectible(&"a".repeat(64), "rgb21-art", issuer.clone());
        let b = collectible(&"b".repeat(64), "rgb21-other", issuer);
        let err = RgbCollectible::validate_transfer(&a, &b).unwrap_err();
        assert!(matches!(err, webycash_asset_core::AssetError::Invariant(_)));
    }

    #[test]
    fn validate_transfer_rejects_issuer_mismatch() {
        let a = collectible(&"a".repeat(64), "rgb21-art", fp(0xaa));
        let b = collectible(&"b".repeat(64), "rgb21-art", fp(0xbb));
        let err = RgbCollectible::validate_transfer(&a, &b).unwrap_err();
        assert!(matches!(err, webycash_asset_core::AssetError::Invariant(_)));
    }

    #[test]
    fn validate_transfer_rejects_both_mismatch() {
        let a = collectible(&"a".repeat(64), "rgb21-art", fp(0xaa));
        let b = collectible(&"b".repeat(64), "rgb21-other", fp(0xbb));
        let err = RgbCollectible::validate_transfer(&a, &b).unwrap_err();
        // Reports the first mismatch encountered (contract_id).
        assert!(matches!(err, webycash_asset_core::AssetError::Invariant(msg) if msg.contains("contract_id")));
    }

    fn rgb_record(contract: &str, issuer: PgpFingerprint) -> RgbFungibleRecord {
        RgbFungibleRecord {
            public_hash: "deadbeef".into(),
            amount_wats: 100,
            spent: false,
            created_at: chrono::Utc::now(),
            spent_at: None,
            origin: RgbOrigin::Issued,
            contract_id: ContractId(contract.into()),
            issuer_fp: issuer,
        }
    }

    #[test]
    fn fungible_verify_issuance_accepts_uniform_batch() {
        let issuer = fp(0xaa);
        let ctx = RgbFungibleIssuance {
            records: vec![
                rgb_record("rgb20-usdc", issuer.clone()),
                rgb_record("rgb20-usdc", issuer.clone()),
                rgb_record("rgb20-usdc", issuer),
            ],
        };
        assert!(RgbFungible::verify_issuance(&ctx).is_ok());
    }

    #[test]
    fn fungible_verify_issuance_accepts_empty_batch() {
        let ctx = RgbFungibleIssuance { records: vec![] };
        assert!(RgbFungible::verify_issuance(&ctx).is_ok());
    }

    #[test]
    fn fungible_verify_issuance_rejects_mixed_contract_ids() {
        let issuer = fp(0xaa);
        let ctx = RgbFungibleIssuance {
            records: vec![
                rgb_record("rgb20-usdc", issuer.clone()),
                rgb_record("rgb20-eth", issuer),
            ],
        };
        let err = RgbFungible::verify_issuance(&ctx).unwrap_err();
        assert!(
            matches!(&err, webycash_asset_core::AssetError::Invariant(msg) if msg.contains("contract_id")),
            "got {err:?}",
        );
    }

    #[test]
    fn fungible_verify_issuance_rejects_mixed_issuers() {
        let ctx = RgbFungibleIssuance {
            records: vec![
                rgb_record("rgb20-usdc", fp(0xaa)),
                rgb_record("rgb20-usdc", fp(0xbb)),
            ],
        };
        let err = RgbFungible::verify_issuance(&ctx).unwrap_err();
        assert!(
            matches!(&err, webycash_asset_core::AssetError::Invariant(msg) if msg.contains("issuer_fp")),
            "got {err:?}",
        );
    }

    #[test]
    fn rgb_origin_displays_lowercase() {
        assert_eq!(RgbOrigin::Mined.to_string(), "mined");
        assert_eq!(RgbOrigin::Issued.to_string(), "issued");
        assert_eq!(RgbOrigin::Replaced.to_string(), "replaced");
    }

    /// Legacy DB rows wrote `origin: "transferred"` from before we
    /// unified to /api/v1/replace. Pin that they still load as Replaced
    /// (one-way: we never emit "transferred" again).
    #[test]
    fn from_fields_accepts_legacy_transferred_alias() {
        let mut fields = std::collections::HashMap::new();
        fields.insert("amount_wats".into(), "100".into());
        fields.insert("origin".into(), "transferred".into());
        fields.insert("contract_id".into(), "rgb20-test".into());
        fields.insert(
            "issuer_fp".into(),
            "aabbccddeeff00112233445566778899aabbccdd".into(),
        );
        let r = <RgbFungibleRecord as webycash_storage::HashRecord>::from_fields("h", &fields)
            .expect("legacy row must load");
        assert_eq!(r.origin, RgbOrigin::Replaced);
    }
}
