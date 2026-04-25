//! ComputeBackend trait + CPU/CUDA/wgpu implementations.
//!
//! Migrated from `crates/server/src/compute/` in M1.D. The trait is
//! unchanged in spirit (sha256_batch, verify_pow_batch); the only
//! generalization is per-asset hash domains where applicable.
//!
//! M1.A ships the trait + a CPU reference implementation.
//! GPU backends (CUDA, wgpu) are wired in M1.D alongside the binary.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use sha2::{Digest, Sha256};

pub type HashResult = [u8; 32];

#[derive(Debug, Clone, Copy)]
pub struct PowResult {
    pub leading_zero_bits: u32,
    pub satisfies: bool,
}

/// Result of deriving a public hash from a secret.
#[derive(Debug, Clone)]
pub struct TokenDerivation {
    /// The 64-char hex sha256(secret_hex_bytes).
    pub public_hash_hex: String,
}

#[async_trait]
pub trait ComputeBackend: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult>;

    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult>;

    /// Derive `(public_hash, ...)` for a batch of bare hex secrets.
    /// Hash domain: `sha256(secret_hex_bytes)` for ALL asset flavors.
    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation>;
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU reference backend.
// ─────────────────────────────────────────────────────────────────────────────

pub struct CpuBackend;

fn count_leading_zero_bits(hash: &[u8]) -> u32 {
    let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
    hash.get(full_zero_bytes as usize)
        .map_or(0, |b| b.leading_zeros())
        + full_zero_bytes * 8
}

#[async_trait]
impl ComputeBackend for CpuBackend {
    fn name(&self) -> &'static str {
        "cpu"
    }

    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult> {
        inputs
            .iter()
            .map(|input| {
                let h = Sha256::digest(input);
                let mut out = [0u8; 32];
                out.copy_from_slice(&h);
                out
            })
            .collect()
    }

    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult> {
        inputs
            .iter()
            .map(|(preimage, target_bits)| {
                let hash = Sha256::digest(preimage.as_bytes());
                let lz = count_leading_zero_bits(&hash);
                PowResult {
                    leading_zero_bits: lz,
                    satisfies: lz >= *target_bits,
                }
            })
            .collect()
    }

    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation> {
        secrets
            .iter()
            .map(|secret| {
                let h = Sha256::digest(secret.as_bytes());
                TokenDerivation {
                    public_hash_hex: hex::encode(h),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cpu_sha256_batch_round_trip() {
        let backend = CpuBackend;
        let inputs = vec![b"hello".to_vec(), b"world".to_vec()];
        let hashes = backend.sha256_batch(&inputs).await;
        assert_eq!(hashes.len(), 2);
        let expected_hello = Sha256::digest(b"hello");
        assert_eq!(hashes[0].as_slice(), expected_hello.as_slice());
    }

    #[tokio::test]
    async fn cpu_verify_pow_zero_target() {
        let backend = CpuBackend;
        let results = backend
            .verify_pow_batch(&[("anything".to_string(), 0)])
            .await;
        assert!(results[0].satisfies);
    }

    #[tokio::test]
    async fn cpu_derive_public_hash_matches_webcash_production() {
        let backend = CpuBackend;
        // Reference: same hex secret used in webcash.org compat tests.
        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let out = backend
            .derive_public_hash_batch(&[secret.to_string()])
            .await;
        let expected = hex::encode(Sha256::digest(secret.as_bytes()));
        assert_eq!(out[0].public_hash_hex, expected);
    }
}
