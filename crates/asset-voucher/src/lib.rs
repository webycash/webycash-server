//! Asset implementation for Vouchers — issuer-namespaced bearer credits.
//!
//! Vouchers are ALWAYS splittable. Replace enforces `(contract_id, issuer_fp)`
//! namespace at the storage layer. No AluVM — vouchers are a static ledger,
//! not a contract VM.

#![forbid(unsafe_code)]

mod token;

pub use token::{PublicVoucher, SecretVoucher, TokenError};

use std::collections::HashMap;

use webycash_asset_core::{
    Amount, Asset, AssetPublic, AssetRecord, AssetSecret, ContractId, IssuedAsset,
    MintableAsset, PgpFingerprint, RecordBuilder, RecordOrigin, Result as AssetResult,
    SplittableAsset,
};

/// Origin tag for a voucher record. Vouchers can be minted via PoW (when the
/// operator enables it) OR via the operator-private signed `/issue` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoucherOrigin {
    /// Created via PoW mining_report.
    Mined,
    /// Created via signed operator /issue.
    Issued,
    /// Created by splitting/replacing existing vouchers.
    Replaced,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoucherRecord {
    pub public_hash: String,
    pub amount_wats: i64,
    pub spent: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub spent_at: Option<chrono::DateTime<chrono::Utc>>,
    pub origin: VoucherOrigin,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

impl AssetRecord for VoucherRecord {}

impl webycash_storage::HashRecord for VoucherRecord {
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
                VoucherOrigin::Mined => "mined",
                VoucherOrigin::Issued => "issued",
                VoucherOrigin::Replaced => "replaced",
            }
            .into(),
        );
        fields.insert("contract_id".into(), self.contract_id.0.clone());
        fields.insert("issuer_fp".into(), self.issuer_fp.0.clone());
    }
    fn from_fields(public_hash: &str, fields: &HashMap<String, String>) -> Option<Self> {
        Some(VoucherRecord {
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
                Some("issued") => VoucherOrigin::Issued,
                Some("replaced") => VoucherOrigin::Replaced,
                _ => VoucherOrigin::Mined,
            },
            contract_id: ContractId(fields.get("contract_id")?.clone()),
            issuer_fp: PgpFingerprint(fields.get("issuer_fp")?.clone()),
        })
    }
}

/// Issuance context: PoW preimage AND/OR operator-signed envelope.
/// At runtime the server checks which mode is active and validates accordingly.
#[derive(Debug, Clone)]
pub struct VoucherIssuance {
    pub records: Vec<VoucherRecord>,
}

pub struct Voucher;

impl Asset for Voucher {
    const NAME: &'static str = "voucher";
    type Secret = SecretVoucher;
    type Public = PublicVoucher;
    type Record = VoucherRecord;

    fn parse_secret(s: &str) -> AssetResult<Self::Secret> {
        SecretVoucher::parse(s).map_err(|e| {
            webycash_asset_core::AssetError::Parse(format!("voucher secret: {e}"))
        })
    }
    fn parse_public(s: &str) -> AssetResult<Self::Public> {
        PublicVoucher::parse(s).map_err(|e| {
            webycash_asset_core::AssetError::Parse(format!("voucher public: {e}"))
        })
    }
    fn to_public(secret: &Self::Secret) -> Self::Public {
        secret.to_public()
    }
}

impl SplittableAsset for Voucher {
    fn amount(secret: &Self::Secret) -> Amount {
        secret.amount
    }
    fn amount_public(public: &Self::Public) -> Amount {
        public.amount
    }
}

impl IssuedAsset for Voucher {
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

impl MintableAsset for Voucher {
    type IssuanceContext = VoucherIssuance;

    fn verify_issuance(_ctx: &Self::IssuanceContext) -> AssetResult<()> {
        // PoW + signature checks happen in `webycash-auth` + handler code.
        Ok(())
    }

    fn build_records(ctx: &Self::IssuanceContext) -> AssetResult<Vec<Self::Record>> {
        Ok(ctx.records.clone())
    }
}

impl RecordBuilder for Voucher {
    fn record_from_secret(secret: &Self::Secret, origin: RecordOrigin) -> Self::Record {
        let public = secret.to_public();
        VoucherRecord {
            public_hash: public.hash,
            amount_wats: secret.amount.wats,
            spent: false,
            created_at: chrono::Utc::now(),
            spent_at: None,
            origin: match origin {
                RecordOrigin::Mined => VoucherOrigin::Mined,
                RecordOrigin::Replaced => VoucherOrigin::Replaced,
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

impl AssetSecret for SecretVoucher {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn secret_hex(&self) -> &str {
        &self.secret
    }
}

impl AssetPublic for PublicVoucher {
    fn wire_form(&self) -> String {
        self.to_string()
    }
    fn public_hash(&self) -> &str {
        &self.hash
    }
}
