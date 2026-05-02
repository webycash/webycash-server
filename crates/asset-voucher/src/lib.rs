//! Asset implementation for Vouchers — issuer-namespaced bearer credits.
//!
//! Vouchers are ALWAYS splittable. Replace enforces `(contract_id, issuer_fp)`
//! namespace at the storage layer. No AluVM — vouchers are a static ledger,
//! not a contract VM.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod token;

pub use token::{PublicVoucher, SecretVoucher, TokenError};

use std::collections::HashMap;

use webycash_asset_core::{
    Amount, Asset, AssetPublic, AssetRecord, AssetSecret, ContractId, IssuedAsset,
    MintableAsset, PgpFingerprint, RecordBuilder, RecordOrigin, ReplaceHook,
    Result as AssetResult, SplittableAsset,
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

impl std::fmt::Display for VoucherOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VoucherOrigin::Mined => f.write_str("mined"),
            VoucherOrigin::Issued => f.write_str("issued"),
            VoucherOrigin::Replaced => f.write_str("replaced"),
        }
    }
}

/// In-DB record for a voucher. Same shape as RGB20 (amount + spent
/// state + namespace + provenance) — vouchers are always splittable
/// bearer credits issued under an `(contract_id, issuer_fp)` pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoucherRecord {
    /// The token's public hash — primary key within its namespace.
    pub public_hash: String,
    /// Amount in atomic units (8-decimal wats).
    pub amount_wats: i64,
    /// `true` once consumed by /replace or /burn.
    pub spent: bool,
    /// Wall-clock when the record was inserted.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Wall-clock when the spent transition fired; `None` while unspent.
    pub spent_at: Option<chrono::DateTime<chrono::Utc>>,
    /// How the record entered the ledger (mined / issued / replaced).
    pub origin: VoucherOrigin,
    /// Issuer-chosen contract id this voucher belongs to.
    pub contract_id: ContractId,
    /// Issuer's PGP V4 fingerprint.
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
///
/// `MintableAsset::IssuanceContext` for the voucher flavor: a batch
/// of pre-built records destined for the ledger via `/api/v1/issue`
/// (after Ed25519 signature verification).
#[derive(Debug, Clone)]
pub struct VoucherIssuance {
    /// Records to insert. Every record must share the same
    /// `(contract_id, issuer_fp)` (verify_issuance enforces this).
    pub records: Vec<VoucherRecord>,
}

/// Zero-sized type identifying the voucher asset flavor.
/// Implements `Asset + SplittableAsset + IssuedAsset + MintableAsset
/// + RecordBuilder` for `Server<Voucher, _>`.
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

/// Voucher servers stay non-conditional by design: vouchers are bearer
/// credits whose semantics do not include preimage / timeout / signature
/// gates. The replace hook is the default no-op accept, matching the
/// constraint in `docs/referee-zkp-based-swap.md` §3.
impl ReplaceHook for Voucher {}

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

    /// Shape-only check: every record in the batch must share the same
    /// `(contract_id, issuer_fp)` so the storage partition stays
    /// single-namespace. PoW + signature checks happen in
    /// `webycash-auth` and the issuer-handler.
    fn verify_issuance(ctx: &Self::IssuanceContext) -> AssetResult<()> {
        let mut iter = ctx.records.iter();
        let Some(first) = iter.next() else {
            return Ok(());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: u8) -> PgpFingerprint {
        PgpFingerprint(format!("{byte:02x}").repeat(20))
    }

    fn record(contract: &str, issuer: PgpFingerprint) -> VoucherRecord {
        VoucherRecord {
            public_hash: "deadbeef".into(),
            amount_wats: 100,
            spent: false,
            created_at: chrono::Utc::now(),
            spent_at: None,
            origin: VoucherOrigin::Issued,
            contract_id: ContractId(contract.into()),
            issuer_fp: issuer,
        }
    }

    #[test]
    fn verify_issuance_accepts_uniform_batch() {
        let issuer = fp(0xaa);
        let ctx = VoucherIssuance {
            records: vec![
                record("credits-q1", issuer.clone()),
                record("credits-q1", issuer.clone()),
                record("credits-q1", issuer),
            ],
        };
        assert!(Voucher::verify_issuance(&ctx).is_ok());
    }

    #[test]
    fn verify_issuance_accepts_empty_batch() {
        let ctx = VoucherIssuance { records: vec![] };
        assert!(Voucher::verify_issuance(&ctx).is_ok());
    }

    #[test]
    fn verify_issuance_rejects_mixed_contract_ids() {
        let issuer = fp(0xaa);
        let ctx = VoucherIssuance {
            records: vec![
                record("credits-q1", issuer.clone()),
                record("credits-q2", issuer),
            ],
        };
        let err = Voucher::verify_issuance(&ctx).unwrap_err();
        assert!(
            matches!(&err, webycash_asset_core::AssetError::Invariant(msg) if msg.contains("contract_id")),
            "got {err:?}",
        );
    }

    #[test]
    fn verify_issuance_rejects_mixed_issuers() {
        let ctx = VoucherIssuance {
            records: vec![
                record("credits-q1", fp(0xaa)),
                record("credits-q1", fp(0xbb)),
            ],
        };
        let err = Voucher::verify_issuance(&ctx).unwrap_err();
        assert!(
            matches!(&err, webycash_asset_core::AssetError::Invariant(msg) if msg.contains("issuer_fp")),
            "got {err:?}",
        );
    }

    #[test]
    fn voucher_origin_displays_lowercase() {
        assert_eq!(VoucherOrigin::Mined.to_string(), "mined");
        assert_eq!(VoucherOrigin::Issued.to_string(), "issued");
        assert_eq!(VoucherOrigin::Replaced.to_string(), "replaced");
    }
}
