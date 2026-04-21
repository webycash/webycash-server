//! Compute backends for cryptographic operations.
//!
//! Modular, pluggable architecture — same pattern as `db::LedgerStore`:
//! - `CpuBackend`: default, portable, zero dependencies
//! - `CudaBackend`: NVIDIA GPU via cudarc (feature = "cuda")
//! - `WgpuBackend`: cross-platform GPU via wgpu (feature = "wgpu-compute")
//!
//! The server selects a backend at startup based on config/hardware detection.
//! All hashing, PoW verification, and token derivation route through this trait.

pub mod cpu;
#[cfg(feature = "cuda")]
pub mod cuda;
#[cfg(feature = "wgpu-compute")]
pub mod wgpu_compute;

use async_trait::async_trait;

/// Result of a batch SHA256 operation.
pub struct HashResult {
    pub hash: [u8; 32],
}

/// Result of a batch PoW verification.
pub struct PowResult {
    pub valid: bool,
    pub leading_zeros: u32,
}

/// Result of a secret-to-public token derivation.
pub struct TokenDerivation {
    pub public_hash: String,
}

/// Compute backend trait — all cryptographic operations routed through this.
///
/// Implementations provide scalar (single-item) and batch (many-item) variants.
/// The batch variants are the performance-critical path: GPU backends dispatch
/// thousands of hashes in a single kernel launch, while CPU backends fall back
/// to parallel iterators via rayon or tokio::spawn_blocking.
#[async_trait]
pub trait ComputeBackend: Send + Sync + 'static {
    /// Human-readable name for logging.
    fn name(&self) -> &'static str;

    /// Maximum efficient batch size for this backend.
    /// GPU: 10_000+, CPU: 1 (no batching benefit).
    fn optimal_batch_size(&self) -> usize;

    /// Single SHA256 hash. Default delegates to batch of 1.
    async fn sha256(&self, data: &[u8]) -> [u8; 32] {
        self.sha256_batch(&[data.to_vec()])
            .await
            .into_iter()
            .next()
            .map(|r| r.hash)
            .unwrap_or([0u8; 32])
    }

    /// Batch SHA256: hash N inputs in parallel.
    /// GPU backends dispatch all N in a single kernel launch.
    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult>;

    /// Verify proof-of-work: SHA256(preimage) must have >= difficulty leading zeros.
    async fn verify_pow(&self, preimage: &str, difficulty_bits: u32) -> PowResult {
        self.verify_pow_batch(&[(preimage.to_string(), difficulty_bits)])
            .await
            .into_iter()
            .next()
            .unwrap_or(PowResult {
                valid: false,
                leading_zeros: 0,
            })
    }

    /// Batch PoW verification: verify N preimages in parallel.
    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult>;

    /// Derive public hash from secret hex string: SHA256(secret_bytes).
    /// This is the token derivation used in replace/mining operations.
    async fn derive_public_hash(&self, secret_hex: &str) -> String {
        self.derive_public_hash_batch(&[secret_hex.to_string()])
            .await
            .into_iter()
            .next()
            .map(|r| r.public_hash)
            .unwrap_or_default()
    }

    /// Batch token derivation: derive N public hashes in parallel.
    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation>;

    /// Count leading zero bits in a 32-byte hash.
    fn leading_zero_bits(&self, hash: &[u8]) -> u32 {
        let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
        hash.get(full_zero_bytes as usize)
            .map_or(0, |b| b.leading_zeros())
            + full_zero_bytes * 8
    }
}

/// Blanket impl for Box<dyn ComputeBackend>.
#[async_trait]
impl ComputeBackend for Box<dyn ComputeBackend> {
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn optimal_batch_size(&self) -> usize {
        (**self).optimal_batch_size()
    }
    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult> {
        (**self).sha256_batch(inputs).await
    }
    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult> {
        (**self).verify_pow_batch(inputs).await
    }
    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation> {
        (**self).derive_public_hash_batch(secrets).await
    }
    fn leading_zero_bits(&self, hash: &[u8]) -> u32 {
        (**self).leading_zero_bits(hash)
    }
}

/// Create the best available compute backend.
pub fn create_backend() -> Box<dyn ComputeBackend> {
    // Try CUDA first (highest throughput)
    #[cfg(feature = "cuda")]
    {
        match cuda::CudaBackend::new() {
            Ok(backend) => {
                tracing::info!("compute backend: CUDA ({})", backend.name());
                return Box::new(backend);
            }
            Err(e) => {
                tracing::warn!("CUDA not available: {e}, falling back");
            }
        }
    }

    // Try wgpu (cross-platform GPU)
    #[cfg(feature = "wgpu-compute")]
    {
        match wgpu_compute::WgpuBackend::new() {
            Ok(backend) => {
                tracing::info!("compute backend: wgpu ({})", backend.name());
                return Box::new(backend);
            }
            Err(e) => {
                tracing::warn!("wgpu not available: {e}, falling back");
            }
        }
    }

    // CPU fallback (always available)
    tracing::info!("compute backend: CPU");
    Box::new(cpu::CpuBackend)
}
