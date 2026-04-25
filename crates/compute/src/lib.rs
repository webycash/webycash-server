//! ComputeBackend trait + CPU/CUDA/wgpu implementations.
//!
//! Migrated from `crates/server/src/compute/` in M1. The trait stays the same
//! (sha256_batch, verify_pow_batch, derive_public_hash_batch); the only
//! generalization is per-asset hash domains where applicable.
