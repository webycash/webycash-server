//! LedgerStore trait + 4 backend implementations.
//!
//! All backends are generic over the asset type (`A: Asset`) and partition
//! token records by `(asset, contract_id, issuer_fp, public_hash)`. For the
//! Webcash flavor, the (contract_id, issuer_fp) slots collapse and the keys
//! emitted match the legacy `token:{public_hash}` shape — preserving testnet
//! Redis schema compatibility.
//!
//! Concrete backend impls (Redis/DynamoDB/FoundationDB/Redis+FDB) are
//! migrated from `crates/server/src/db/` during M1.D when `server-core`
//! comes up. This crate currently ships:
//!   - the generic `LedgerStore<A>` trait
//!   - shared record/op/result types
//!   - a `KeyStrategy` trait + Webcash legacy specialization

#![forbid(unsafe_code)]

use std::marker::PhantomData;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use webycash_asset_core::{Asset, ContractId, IssuedAsset, PgpFingerprint};

// ─────────────────────────────────────────────────────────────────────────────
// Audit + stats record types (shared across all asset flavors)
// ─────────────────────────────────────────────────────────────────────────────

/// Audit record for a replacement (transfer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplacementRecord {
    pub id: String,
    pub input_hashes: Vec<String>,
    pub output_hashes: Vec<String>,
    pub total_amount_wats: i64,
    pub created_at: DateTime<Utc>,
}

/// Audit record for a burn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnRecord {
    pub id: String,
    pub public_hash: String,
    pub amount_wats: i64,
    pub burned_at: DateTime<Utc>,
}

/// Economy statistics, derived from mining state. Shape is
/// asset-flavor-uniform; specific values come from per-flavor mining
/// configs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EconomyStats {
    pub total_circulation_wats: i64,
    pub mining_reports_count: u64,
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub mining_amount_wats: i64,
    pub subsidy_amount_wats: i64,
}

/// Mining state — current difficulty/epoch/circulation. Source of truth
/// for `EconomyStats`. Lives in storage so all flavors share the schema;
/// the difficulty-adjustment algorithms in `webycash-mining` mutate it.
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

/// One replace operation in a batch. Generic over the per-asset record.
#[derive(Debug, Clone)]
pub struct ReplaceOp<R> {
    pub inputs: Vec<String>,
    pub outputs: Vec<R>,
    pub record: ReplacementRecord,
}

/// Result of a single replace operation within a batch.
#[derive(Debug)]
pub enum ReplaceResult {
    Ok,
    Failed(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// LedgerStore<A> — batch-native, asset-generic.
// ─────────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LedgerStore<A: Asset>: Send + Sync + 'static {
    /// Insert tokens. Each must have a unique hash within its namespace —
    /// duplicates fail. Backend pipelines all inserts in minimal round-trips.
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()>;

    /// Look up tokens by public hash within a namespace. Returns in same
    /// order as input. Backend pipelines lookups.
    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>>;

    /// Check spent status for multiple tokens.
    /// Returns (hash, Option<bool>) in same order: None = not found.
    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>>;

    /// Execute a batch of atomic replace operations within the same
    /// namespace. Each op independently succeeds or fails.
    async fn batch_replace(
        &self,
        ns: &Namespace,
        ops: &[ReplaceOp<A::Record>],
    ) -> Vec<ReplaceResult>;

    /// Burn multiple tokens within a namespace.
    async fn batch_burn(
        &self,
        ns: &Namespace,
        ops: &[(String, BurnRecord)],
    ) -> anyhow::Result<()>;

    /// Get current mining state (per asset).
    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>>;

    /// Update mining state (per asset).
    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()>;

    /// Get economy statistics. Default: derived from mining state.
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
// Namespacing: (contract_id, issuer_fp). For Webcash both are absent.
// ─────────────────────────────────────────────────────────────────────────────

/// Storage namespace identifier.
///
/// - `Webcash` flavor uses `Namespace::default()` (no contract, no issuer).
/// - RGB / Voucher flavors use `Namespace::scoped(contract, issuer)`.
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

/// Helper for `IssuedAsset`s: extract a Namespace from a parsed secret.
pub fn namespace_for_secret<A>(secret: &A::Secret) -> Namespace
where
    A: IssuedAsset,
{
    Namespace::scoped(A::contract_id(secret).clone(), A::issuer(secret).clone())
}

// ─────────────────────────────────────────────────────────────────────────────
// KeyStrategy: how a backend turns (asset_name, namespace, hash) into a key.
// ─────────────────────────────────────────────────────────────────────────────

/// Strategy for serialising a `(asset, namespace, public_hash)` triple into
/// a backend-native key (Redis HASH name, DynamoDB PK/SK, FDB tuple, etc.).
///
/// The Webcash legacy strategy emits `token:{public_hash}` regardless of
/// asset name, preserving on-disk compatibility with deployed testnet
/// Redis instances. The general strategy emits
/// `{asset}:{contract}:{issuer}:token:{hash}`.
pub trait KeyStrategy: Send + Sync + 'static {
    fn token_key(&self, asset_name: &str, ns: &Namespace, public_hash: &str) -> String;
    fn replacement_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String;
    fn burn_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String;
    fn mining_state_key(&self, asset_name: &str) -> String;
}

/// Webcash-only key strategy: ignores asset_name and namespace, emits the
/// legacy keys. Activated when `A: Asset` has `NAME == "webcash"`.
pub struct WebcashLegacyKeys;

impl KeyStrategy for WebcashLegacyKeys {
    fn token_key(&self, _asset_name: &str, _ns: &Namespace, public_hash: &str) -> String {
        format!("token:{public_hash}")
    }
    fn replacement_key(&self, _asset_name: &str, _ns: &Namespace, op_id: &str) -> String {
        format!("replacement:{op_id}")
    }
    fn burn_key(&self, _asset_name: &str, _ns: &Namespace, op_id: &str) -> String {
        format!("burn:{op_id}")
    }
    fn mining_state_key(&self, _asset_name: &str) -> String {
        "mining_state".to_string()
    }
}

/// Asset-namespaced key strategy: includes asset, contract, issuer in keys.
/// Used by RGB and Voucher.
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
        format!("{asset_name}:{contract}:{issuer}:replacement:{op_id}")
    }
    fn burn_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String {
        let contract = ns.contract_id.as_ref().map(|c| c.0.as_str()).unwrap_or("_");
        let issuer = ns.issuer_fp.as_ref().map(|i| i.0.as_str()).unwrap_or("_");
        format!("{asset_name}:{contract}:{issuer}:burn:{op_id}")
    }
    fn mining_state_key(&self, asset_name: &str) -> String {
        format!("{asset_name}:mining_state")
    }
}

/// Marker pointing each asset at the right key strategy.
///
/// Webcash gets `WebcashLegacyKeys` (compat); everything else gets
/// `NamespacedKeys`. Server flavors instantiate `Backend<A, Self::Keys>`
/// with the appropriate strategy.
pub struct Strategy<A>(PhantomData<A>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcash_legacy_keys_match_legacy_format() {
        let s = WebcashLegacyKeys;
        let ns = Namespace::unscoped();
        assert_eq!(s.token_key("webcash", &ns, "abc"), "token:abc");
        assert_eq!(
            s.replacement_key("webcash", &ns, "op1"),
            "replacement:op1"
        );
        assert_eq!(s.burn_key("webcash", &ns, "b1"), "burn:b1");
        assert_eq!(s.mining_state_key("webcash"), "mining_state");
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
        assert_eq!(
            s.token_key("voucher", &ns, "h1"),
            "voucher:_:_:token:h1"
        );
    }
}
