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

/// Per-input result of a `verify_pow_batch` call. Carries both the
/// observed leading-zero count AND the boolean comparison against the
/// configured target (so callers can sort/rank "near-misses").
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

/// Pluggable hash + PoW + derive backend. CPU reference impl ships
/// in this crate; CUDA / wgpu impls re-introduced in M1. Every method
/// is batch-native — the per-input shape preserves order so callers
/// can zip results back with their inputs.
#[async_trait]
pub trait ComputeBackend: Send + Sync + 'static {
    /// Backend name for logging (e.g. `"cpu"`, `"cuda"`, `"wgpu"`).
    fn name(&self) -> &'static str;

    /// SHA256 every input. Returns one 32-byte result per input,
    /// in input order.
    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult>;

    /// SHA256 each preimage and report whether the resulting digest
    /// has at least the input's target leading-zero bits. Returns one
    /// `PowResult` per input, in input order.
    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult>;

    /// Derive `(public_hash, ...)` for a batch of bare hex secrets.
    /// Hash domain: `sha256(secret_hex_bytes)` for ALL asset flavors.
    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation>;
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU reference backend.
// ─────────────────────────────────────────────────────────────────────────────

/// Reference CPU implementation. Pure sha2 + leading-zero math; no
/// hardware acceleration. Fast enough that the property tests in
/// `webycash-conformance` pin agreement with the canonical sha2
/// crate over arbitrary input.
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

    use proptest::prelude::*;

    /// Block on a tokio future from inside proptest's sync runner.
    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
        }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Batch length always matches input length.
        #[test]
        fn cpu_sha256_batch_length_matches_input(
            inputs in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=256), 0..=32),
        ) {
            let result = run(async {
                CpuBackend.sha256_batch(&inputs).await
            });
            prop_assert_eq!(result.len(), inputs.len());
        }

        /// Each batch element equals the canonical sha2::Sha256 of the
        /// same input — and order is preserved.
        #[test]
        fn cpu_sha256_batch_agrees_with_sha2(
            inputs in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=128), 0..=16),
        ) {
            let result = run(async {
                CpuBackend.sha256_batch(&inputs).await
            });
            for (i, (got, src)) in result.iter().zip(inputs.iter()).enumerate() {
                let want: [u8; 32] = Sha256::digest(src).into();
                prop_assert_eq!(got, &want, "index {} diverges", i);
            }
        }

        /// PoW batch agrees with `webycash_mining::verify_pow` shape:
        /// `satisfies == leading_zero_bits >= target`.
        #[test]
        fn cpu_verify_pow_batch_self_consistent(
            preimages in prop::collection::vec(any::<String>(), 0..=8),
            target_bits in 0u32..=12,
        ) {
            let inputs: Vec<(String, u32)> =
                preimages.iter().map(|s| (s.clone(), target_bits)).collect();
            let results = run(async {
                CpuBackend.verify_pow_batch(&inputs).await
            });
            prop_assert_eq!(results.len(), inputs.len());
            for (r, (s, t)) in results.iter().zip(inputs.iter()) {
                prop_assert_eq!(r.satisfies, r.leading_zero_bits >= *t);
                // And the leading_zero_bits matches a fresh sha2 hash.
                let h = Sha256::digest(s.as_bytes());
                let lz = count_leading_zero_bits(&h);
                prop_assert_eq!(r.leading_zero_bits, lz);
            }
        }

        /// `derive_public_hash_batch` matches `sha256(secret_bytes)` for
        /// every secret (uniform hash domain across asset flavors).
        #[test]
        fn cpu_derive_public_hash_uniform_with_sha2(
            secrets in prop::collection::vec(any::<String>(), 0..=16),
        ) {
            let derivations = run(async {
                CpuBackend.derive_public_hash_batch(&secrets).await
            });
            prop_assert_eq!(derivations.len(), secrets.len());
            for (d, s) in derivations.iter().zip(secrets.iter()) {
                let want = hex::encode(Sha256::digest(s.as_bytes()));
                prop_assert_eq!(&d.public_hash_hex, &want);
            }
        }
    }
}
