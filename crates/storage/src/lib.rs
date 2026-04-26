//! LedgerStore trait + 4 backend implementations.
//!
//! All backends are generic over the asset type (`A: Asset`) and partition
//! token records by `(asset, contract_id, issuer_fp, public_hash)`. For the
//! Webcash flavor, the (contract_id, issuer_fp) slots collapse and the keys
//! emitted match the legacy `token:{public_hash}` shape — preserving testnet
//! Redis schema compatibility.
//!
//! Available backends (cargo features):
//!   - `redis`     → `redis_backend::RedisStore<A, K>`
//!   - `dynamodb`  → planned in M1.D follow-up
//!   - `fdb`       → planned in M1.D follow-up
//!   - `redis-fdb` → composite, planned

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use webycash_asset_core::{Asset, ContractId, IssuedAsset, PgpFingerprint};

#[cfg(feature = "redis")]
pub mod redis_backend;
#[cfg(feature = "dynamodb")]
pub mod dynamodb_backend;
#[cfg(feature = "fdb")]
pub mod fdb_backend;
#[cfg(all(feature = "redis", feature = "fdb"))]
pub mod redis_fdb_backend;

// ─────────────────────────────────────────────────────────────────────────────
// Audit + stats record types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplacementRecord {
    pub id: String,
    pub input_hashes: Vec<String>,
    pub output_hashes: Vec<String>,
    pub total_amount_wats: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnRecord {
    pub id: String,
    pub public_hash: String,
    pub amount_wats: i64,
    pub burned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EconomyStats {
    pub total_circulation_wats: i64,
    pub mining_reports_count: u64,
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub mining_amount_wats: i64,
    pub subsidy_amount_wats: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiningState {
    pub total_circulation_wats: i64,
    pub mining_reports_count: u64,
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub mining_amount_wats: i64,
    pub subsidy_amount_wats: i64,
    pub epoch_started_at: DateTime<Utc>,
    pub aggregate_work: u64,
}

#[derive(Debug, Clone)]
pub struct ReplaceOp<R> {
    pub inputs: Vec<String>,
    pub outputs: Vec<R>,
    pub record: ReplacementRecord,
}

#[derive(Debug)]
pub enum ReplaceResult {
    Ok,
    Failed(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// HashRecord — codec between asset records and Redis HASH field maps.
// ─────────────────────────────────────────────────────────────────────────────

/// A record that can be stored as a Redis HASH (or DynamoDB attribute map).
///
/// Each backend can pick a serialization strategy: HASH fields (preserves
/// legacy webcash testnet compat), JSON in a single field, or strict-types
/// for RGB. Webcash uses the legacy field-per-field layout.
pub trait HashRecord: Sized + Send + Sync {
    fn public_hash(&self) -> &str;
    fn amount_wats(&self) -> i64;

    /// Returns the storage namespace for this record. Webcash records
    /// return `Namespace::unscoped()`; RGB and Voucher records return
    /// `Namespace::scoped(contract_id, issuer_fp)`.
    fn namespace(&self) -> Namespace {
        Namespace::unscoped()
    }

    /// Write fields into a backend-neutral string map.
    fn to_fields(&self, fields: &mut HashMap<String, String>);

    /// Reconstruct from a backend-neutral string map keyed by public hash.
    fn from_fields(public_hash: &str, fields: &HashMap<String, String>) -> Option<Self>;
}

// ─────────────────────────────────────────────────────────────────────────────
// LedgerStore<A> — batch-native, asset-generic.
// ─────────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LedgerStore<A: Asset>: Send + Sync + 'static {
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()>;

    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>>;

    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>>;

    async fn batch_replace(
        &self,
        ns: &Namespace,
        ops: &[ReplaceOp<A::Record>],
    ) -> Vec<ReplaceResult>;

    async fn batch_burn(
        &self,
        ns: &Namespace,
        ops: &[(String, BurnRecord)],
    ) -> anyhow::Result<()>;

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>>;

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()>;

    async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        Ok(self
            .get_mining_state()
            .await?
            .map(|s| EconomyStats {
                total_circulation_wats: s.total_circulation_wats,
                mining_reports_count: s.mining_reports_count,
                difficulty_target_bits: s.difficulty_target_bits,
                epoch: s.epoch,
                mining_amount_wats: s.mining_amount_wats,
                subsidy_amount_wats: s.subsidy_amount_wats,
            })
            .unwrap_or_default())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Namespacing
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Namespace {
    pub contract_id: Option<ContractId>,
    pub issuer_fp: Option<PgpFingerprint>,
}

impl Namespace {
    pub fn unscoped() -> Self {
        Self::default()
    }
    pub fn scoped(contract_id: ContractId, issuer_fp: PgpFingerprint) -> Self {
        Self {
            contract_id: Some(contract_id),
            issuer_fp: Some(issuer_fp),
        }
    }
}

pub fn namespace_for_secret<A>(secret: &A::Secret) -> Namespace
where
    A: IssuedAsset,
{
    Namespace::scoped(A::contract_id(secret).clone(), A::issuer(secret).clone())
}

// ─────────────────────────────────────────────────────────────────────────────
// KeyStrategy
// ─────────────────────────────────────────────────────────────────────────────

pub trait KeyStrategy: Send + Sync + 'static {
    fn token_key(&self, asset_name: &str, ns: &Namespace, public_hash: &str) -> String;
    fn replacement_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String;
    fn burn_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String;
    fn mining_state_key(&self, asset_name: &str) -> String;
}

/// Frozen-schema key strategy used ONLY by the Webcash flavor. Emits
/// the bare `token:{hash}` / `audit:replace:{op}` / `audit:burn:{op}` /
/// `mining:state` keys deployed testnet Redis instances were
/// initialised with — wire-protocol-frozen, the asset name and
/// namespace inputs are intentionally ignored.
///
/// All other flavors (RGB20, RGB21, Voucher) use [`NamespacedKeys`]
/// to partition by `(asset, contract_id, issuer_fp)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WebcashLegacyKeys;

impl KeyStrategy for WebcashLegacyKeys {
    fn token_key(&self, _asset_name: &str, _ns: &Namespace, public_hash: &str) -> String {
        format!("token:{public_hash}")
    }
    fn replacement_key(&self, _asset_name: &str, _ns: &Namespace, op_id: &str) -> String {
        format!("audit:replace:{op_id}")
    }
    fn burn_key(&self, _asset_name: &str, _ns: &Namespace, op_id: &str) -> String {
        format!("audit:burn:{op_id}")
    }
    fn mining_state_key(&self, _asset_name: &str) -> String {
        "mining:state".to_string()
    }
}

/// Key strategy for issuer-namespaced flavors (RGB20, RGB21, Voucher).
/// Emits `{asset}:{contract_id}:{issuer_fp}:token:{hash}` so that
/// scans / aggregations stay within the issuer's namespace by
/// construction. Cross-asset / cross-namespace collisions are
/// statically impossible (8 storage-key proptest invariants pin this).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NamespacedKeys;

impl KeyStrategy for NamespacedKeys {
    fn token_key(&self, asset_name: &str, ns: &Namespace, public_hash: &str) -> String {
        let contract = ns.contract_id.as_ref().map(|c| c.0.as_str()).unwrap_or("_");
        let issuer = ns.issuer_fp.as_ref().map(|i| i.0.as_str()).unwrap_or("_");
        format!("{asset_name}:{contract}:{issuer}:token:{public_hash}")
    }
    fn replacement_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String {
        let contract = ns.contract_id.as_ref().map(|c| c.0.as_str()).unwrap_or("_");
        let issuer = ns.issuer_fp.as_ref().map(|i| i.0.as_str()).unwrap_or("_");
        format!("{asset_name}:{contract}:{issuer}:audit:replace:{op_id}")
    }
    fn burn_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String {
        let contract = ns.contract_id.as_ref().map(|c| c.0.as_str()).unwrap_or("_");
        let issuer = ns.issuer_fp.as_ref().map(|i| i.0.as_str()).unwrap_or("_");
        format!("{asset_name}:{contract}:{issuer}:audit:burn:{op_id}")
    }
    fn mining_state_key(&self, asset_name: &str) -> String {
        format!("{asset_name}:mining:state")
    }
}

pub struct Strategy<A>(PhantomData<A>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcash_legacy_keys_match_legacy_format() {
        let s = WebcashLegacyKeys;
        let ns = Namespace::unscoped();
        assert_eq!(s.token_key("webcash", &ns, "abc"), "token:abc");
        assert_eq!(s.replacement_key("webcash", &ns, "op1"), "audit:replace:op1");
        assert_eq!(s.burn_key("webcash", &ns, "b1"), "audit:burn:b1");
        assert_eq!(s.mining_state_key("webcash"), "mining:state");
    }

    #[test]
    fn namespaced_keys_include_asset_contract_issuer() {
        let s = NamespacedKeys;
        let ns = Namespace::scoped(
            ContractId("rgb20-usdc".into()),
            PgpFingerprint("aabbccddeeff00112233445566778899aabbccdd".into()),
        );
        assert_eq!(
            s.token_key("rgb", &ns, "deadbeef"),
            "rgb:rgb20-usdc:aabbccddeeff00112233445566778899aabbccdd:token:deadbeef"
        );
    }

    #[test]
    fn namespaced_keys_handle_unscoped() {
        let s = NamespacedKeys;
        let ns = Namespace::unscoped();
        assert_eq!(s.token_key("voucher", &ns, "h1"), "voucher:_:_:token:h1");
    }
}
