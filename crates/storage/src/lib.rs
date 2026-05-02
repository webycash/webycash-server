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
#![warn(missing_docs)]

use std::collections::HashMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use webycash_asset_core::{Asset, ContractId, IssuedAsset, PgpFingerprint};

#[cfg(feature = "dynamodb")]
pub mod dynamodb_backend;
#[cfg(feature = "fdb")]
pub mod fdb_backend;
#[cfg(feature = "redis")]
pub mod redis_backend;
#[cfg(all(feature = "redis", feature = "fdb"))]
pub mod redis_fdb_backend;

// ─────────────────────────────────────────────────────────────────────────────
// Audit + stats record types
// ─────────────────────────────────────────────────────────────────────────────

/// Audit trail entry written every time `/api/v1/replace` succeeds.
/// Records the input → output hash mapping + the conserved total
/// amount so an operator can reconstruct the chain of state moves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplacementRecord {
    /// Unique opaque op id (UUID-v4 in the production handlers).
    pub id: String,
    /// Public hashes of the inputs being marked spent.
    pub input_hashes: Vec<String>,
    /// Public hashes of the outputs being inserted.
    pub output_hashes: Vec<String>,
    /// Conserved total (sum of input amounts == sum of output amounts).
    pub total_amount_wats: i64,
    /// Wall-clock when the replace committed.
    pub created_at: DateTime<Utc>,
}

/// Audit trail entry written every time `/api/v1/burn` succeeds.
/// Burns are terminal — the public_hash transitions to spent without
/// a replacement output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnRecord {
    /// Unique opaque op id (UUID-v4 in the production handlers).
    pub id: String,
    /// The public hash that was destroyed.
    pub public_hash: String,
    /// Amount that was destroyed (carried for audit purposes; does
    /// NOT re-enter circulation).
    pub amount_wats: i64,
    /// Wall-clock when the burn committed.
    pub burned_at: DateTime<Utc>,
}

/// Snapshot returned by `/api/v1/stats`. Read-only, derived from
/// MiningState plus aggregations across the token store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EconomyStats {
    /// Sum of all unspent token amounts.
    pub total_circulation_wats: i64,
    /// Cumulative count of accepted mining_report submissions.
    pub mining_reports_count: u64,
    /// Current PoW target in leading-zero bits.
    pub difficulty_target_bits: u32,
    /// Difficulty epoch counter (incremented at each adjustment).
    pub epoch: u32,
    /// Mining reward per accepted report (wats).
    pub mining_amount_wats: i64,
    /// Subsidy per accepted report (wats).
    pub subsidy_amount_wats: i64,
}

/// Persisted mining-economy state: difficulty, epoch boundaries,
/// per-epoch mining/subsidy targets, accumulated proof-of-work since
/// genesis. Updated by the mining_report handler under a Lua-script
/// (Redis) / TransactWriteItem (DynamoDB) atomic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiningState {
    /// Sum of all unspent token amounts since genesis.
    pub total_circulation_wats: i64,
    /// Cumulative count of accepted mining_report submissions.
    pub mining_reports_count: u64,
    /// Current PoW target in leading-zero bits.
    pub difficulty_target_bits: u32,
    /// Difficulty epoch counter.
    pub epoch: u32,
    /// Mining reward per accepted report this epoch.
    pub mining_amount_wats: i64,
    /// Subsidy per accepted report this epoch.
    pub subsidy_amount_wats: i64,
    /// Wall-clock when the current epoch started.
    pub epoch_started_at: DateTime<Utc>,
    /// Accumulated proof-of-work since genesis (sum of leading-zero
    /// bits across all accepted reports).
    pub aggregate_work: u64,
}

/// Atomic replace request: a batch of input hashes to mark spent +
/// a batch of output records to insert + the audit envelope.
/// Backends commit all-or-nothing.
#[derive(Debug, Clone)]
pub struct ReplaceOp<R> {
    /// Public hashes to mark spent.
    pub inputs: Vec<String>,
    /// Records to insert.
    pub outputs: Vec<R>,
    /// Audit envelope written alongside the state move.
    pub record: ReplacementRecord,
}

/// Outcome of a `LedgerStore::replace_atomic` call. `Failed` carries a
/// human-readable diagnostic the handler relays to the client.
#[derive(Debug)]
pub enum ReplaceResult {
    /// All-or-nothing replace committed.
    Ok,
    /// Replace was rejected; carries the diagnostic to surface.
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
    /// The token's public hash — primary key within its namespace.
    fn public_hash(&self) -> &str;
    /// Amount in atomic units (8-decimal wats). Collectibles return 0
    /// to keep the HASH layout uniform with fungible records.
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

/// Asset-generic batch-native ledger store. Implemented by each
/// backend (Redis, DynamoDB, FoundationDB, Redis+FDB) over the asset
/// flavor `A`. Every method takes a batch — single-op callers pass a
/// 1-element slice. The KeyStrategy trait object held by the impl
/// decides whether keys are wire-frozen (Webcash) or namespaced
/// (RGB / Voucher).
// `#[async_trait]` generates internal helper methods that don't show
// up in the trait's user-visible API but trip `missing_docs`.
// Suppress at the trait level — every user-facing method below
// carries rustdoc.
#[async_trait]
#[allow(missing_docs)]
pub trait LedgerStore<A: Asset>: Send + Sync + 'static {
    /// Insert a batch of fresh records (mining_report or signed
    /// /issue path). Backends fail-fast on the first conflict.
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()>;

    /// Fetch full records for the given hashes within a namespace.
    /// Slot is `None` for hashes that don't exist.
    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>>;

    /// Light-weight spent-state probe used by `/api/v1/health_check`.
    /// Returns `(hash, Some(spent)) | (hash, None)` — `None` for
    /// unknown hashes (matches webcash.org production semantics).
    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>>;

    /// All-or-nothing replace: per-op atomically marks every input
    /// hash spent and inserts every output record. The batch is fired
    /// as a single Redis Lua eval / DynamoDB TransactWriteItems / FDB
    /// transaction. Returns one `ReplaceResult` per op in input order.
    async fn batch_replace(
        &self,
        ns: &Namespace,
        ops: &[ReplaceOp<A::Record>],
    ) -> Vec<ReplaceResult>;

    /// Permanent destruction. Each `(hash, BurnRecord)` pair marks
    /// the hash spent without inserting a replacement. All-or-nothing
    /// per backend's atomic primitive.
    async fn batch_burn(&self, ns: &Namespace, ops: &[(String, BurnRecord)]) -> anyhow::Result<()>;

    /// Read the current `MiningState` singleton. `None` on a fresh
    /// store before the first mining_report.
    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>>;

    /// Atomic compare-and-swap on `MiningState`. Backends use a
    /// per-flavor key under their atomic primitive (Lua / TransactWriteItem).
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

/// Storage partitioning key. Webcash collapses to `unscoped`;
/// RGB / Voucher use `scoped(contract_id, issuer_fp)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Namespace {
    /// Contract id when the asset is issuer-namespaced; `None` for
    /// flat-keyspace flavors (Webcash).
    pub contract_id: Option<ContractId>,
    /// Issuer's PGP V4 fingerprint when issuer-namespaced; `None`
    /// otherwise.
    pub issuer_fp: Option<PgpFingerprint>,
}

impl Namespace {
    /// Webcash flavor — no contract or issuer scoping. Storage keys
    /// collapse to the legacy `token:{hash}` shape via
    /// `WebcashLegacyKeys`.
    pub fn unscoped() -> Self {
        Self::default()
    }

    /// Issued-asset flavors (RGB, Voucher) — every record lives in
    /// the `(contract_id, issuer_fp)` partition so cross-namespace
    /// replaces are statically rejected at the storage key level.
    pub fn scoped(contract_id: ContractId, issuer_fp: PgpFingerprint) -> Self {
        Self {
            contract_id: Some(contract_id),
            issuer_fp: Some(issuer_fp),
        }
    }
}

/// Lift an `IssuedAsset` secret into the `(contract_id, issuer_fp)`
/// namespace it lives in. Used by handlers that need the namespace
/// before they have the full record (e.g. the cross-namespace check
/// in /api/v1/replace).
pub fn namespace_for_secret<A>(secret: &A::Secret) -> Namespace
where
    A: IssuedAsset,
{
    Namespace::scoped(A::contract_id(secret).clone(), A::issuer(secret).clone())
}

// ─────────────────────────────────────────────────────────────────────────────
// KeyStrategy
// ─────────────────────────────────────────────────────────────────────────────

/// How storage keys are shaped per asset flavor. Implementations decide
/// whether a record's storage key includes the namespace or not. Two
/// concrete impls ship: `WebcashLegacyKeys` (frozen wire-format
/// schema; ignores namespace) and `NamespacedKeys` (partitions by
/// `(asset, contract_id, issuer_fp)`).
pub trait KeyStrategy: Send + Sync + 'static {
    /// Storage key for a token's HASH record.
    fn token_key(&self, asset_name: &str, ns: &Namespace, public_hash: &str) -> String;
    /// Storage key for a `/api/v1/replace` audit record.
    fn replacement_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String;
    /// Storage key for a `/api/v1/burn` audit record.
    fn burn_key(&self, asset_name: &str, ns: &Namespace, op_id: &str) -> String;
    /// Storage key for the per-asset MiningState singleton.
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

/// PhantomData wrapper used by backends to carry the asset-flavor
/// type parameter without storing a concrete instance. Public so
/// downstream backend impls can use it.
pub struct Strategy<A>(
    /// Marker only.
    pub PhantomData<A>,
);

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
            "audit:replace:op1"
        );
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
