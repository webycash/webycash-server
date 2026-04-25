//! LedgerStore trait + 4 backend implementations.
//!
//! All backends are generic over the asset type (`A: Asset`) and partition
//! token records by `(asset, contract_id, issuer_fp, public_hash)`. For the
//! Webcash flavor, the (contract_id, issuer_fp) slots collapse and the keys
//! emitted match the legacy `token:{public_hash}` shape — preserving testnet
//! Redis schema compatibility.
//!
//! Implementation lands in M1 (migrating from `crates/server/src/db/`).
