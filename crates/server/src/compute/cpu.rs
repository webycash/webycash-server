//! CPU compute backend — portable, zero dependencies beyond sha2.
//!
//! Uses tokio::spawn_blocking for batch operations to avoid
//! blocking the async runtime on large hash batches.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use super::{ComputeBackend, HashResult, PowResult, TokenDerivation};

/// CPU-based compute backend. Always available, no special hardware.
pub struct CpuBackend;

#[async_trait]
impl ComputeBackend for CpuBackend {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn optimal_batch_size(&self) -> usize {
        1 // No batching advantage on CPU — each hash is independent
    }

    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult> {
        let inputs = inputs.to_vec();
        tokio::task::spawn_blocking(move || {
            inputs
                .iter()
                .map(|input| {
                    let hash: [u8; 32] = Sha256::digest(input).into();
                    HashResult { hash }
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult> {
        let inputs = inputs.to_vec();
        tokio::task::spawn_blocking(move || {
            inputs
                .iter()
                .map(|(preimage, difficulty)| {
                    let hash = Sha256::digest(preimage.as_bytes());
                    let zeros = leading_zero_bits_cpu(&hash);
                    PowResult {
                        valid: zeros >= *difficulty,
                        leading_zeros: zeros,
                    }
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation> {
        let secrets = secrets.to_vec();
        tokio::task::spawn_blocking(move || {
            secrets
                .iter()
                .map(|secret| {
                    let hash = Sha256::digest(secret.as_bytes());
                    TokenDerivation {
                        public_hash: hex::encode(hash),
                    }
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }
}

/// Optimized leading zero bit count for CPU — identical to protocol::mining::leading_zero_bits.
fn leading_zero_bits_cpu(hash: &[u8]) -> u32 {
    let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
    hash.get(full_zero_bytes as usize)
        .map_or(0, |b| b.leading_zeros())
        + full_zero_bytes * 8
}
