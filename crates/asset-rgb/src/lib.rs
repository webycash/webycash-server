//! Asset implementation for RGB contracts.
//!
//! Supports RGB20 (fungible, splittable, implements `SplittableAsset`) and
//! RGB21 (NFT, non-splittable, implements `TransferableAsset`).
//! Issuer-namespaced — every secret carries the issuer's PGP fingerprint and
//! a contract_id. Replace stays within `(contract_id, issuer_fp)`.
//!
//! Implementation lands in M3, after the WASM viability gate validates the
//! RGB ecosystem crates compile to `wasm32-unknown-unknown`.
