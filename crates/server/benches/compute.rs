//! Compute backend benchmarks.
//!
//! Measures SHA256 hash throughput on CPU vs GPU (wgpu).
//! Run: cargo bench --bench compute --features wgpu-compute

use std::time::Instant;

use sha2::{Digest, Sha256};
use webycash_server::compute::{self, ComputeBackend};

fn random_inputs(count: usize, len: usize) -> Vec<Vec<u8>> {
    use rand::Rng;
    (0..count)
        .map(|_| {
            let bytes: Vec<u8> = (0..len).map(|_| rand::thread_rng().gen()).collect();
            bytes
        })
        .collect()
}

async fn bench_backend(name: &str, backend: &dyn ComputeBackend, inputs: &[Vec<u8>]) {
    let n = inputs.len();

    // Warmup
    backend.sha256_batch(&inputs[..n.min(100)]).await;

    let start = Instant::now();
    let results = backend.sha256_batch(inputs).await;
    let elapsed = start.elapsed();

    let hps = n as f64 / elapsed.as_secs_f64();
    println!(
        "  {name:<30} {n:>8} hashes  {:.3}s  {:>12.0} H/s  {:.1}us/hash",
        elapsed.as_secs_f64(),
        hps,
        elapsed.as_micros() as f64 / n as f64,
    );

    // Verify correctness: check first result matches CPU SHA256
    if !results.is_empty() {
        let expected: [u8; 32] = Sha256::digest(&inputs[0]).into();
        assert_eq!(
            results[0].hash,
            expected,
            "GPU hash mismatch! GPU={} CPU={}",
            hex::encode(results[0].hash),
            hex::encode(expected)
        );
    }
}

async fn bench_pow(name: &str, backend: &dyn ComputeBackend, preimages: &[(String, u32)]) {
    let n = preimages.len();

    let start = Instant::now();
    let results = backend.verify_pow_batch(preimages).await;
    let elapsed = start.elapsed();

    let vps = n as f64 / elapsed.as_secs_f64();
    let valid_count = results.iter().filter(|r| r.valid).count();
    println!(
        "  {name:<30} {n:>8} verifs  {:.3}s  {:>12.0} V/s  {valid_count} valid",
        elapsed.as_secs_f64(),
        vps,
    );
}

#[tokio::main]
async fn main() {
    println!("\n{}", "=".repeat(80));
    println!("  Compute Backend Benchmarks");
    println!("{}\n", "=".repeat(80));

    // CPU backend
    let cpu = compute::cpu::CpuBackend;
    println!("--- CPU backend ---");

    for size in [100, 1000, 10000, 50000] {
        let inputs = random_inputs(size, 200); // 200 bytes ~ typical preimage size
        bench_backend(&format!("SHA256 batch (n={size})"), &cpu, &inputs).await;
    }

    // PoW verification
    println!();
    let preimages: Vec<(String, u32)> = (0..10000)
        .map(|i| {
            (
                format!("test_preimage_{i}_padding_data_to_make_it_realistic"),
                0,
            )
        })
        .collect();
    bench_pow("PoW verify (n=10000, d=0)", &cpu, &preimages).await;

    // Token derivation
    println!();
    let secrets: Vec<String> = (0..10000)
        .map(|_| {
            use rand::Rng;
            let bytes: [u8; 32] = rand::thread_rng().gen();
            hex::encode(bytes)
        })
        .collect();
    let start = Instant::now();
    let derivations = cpu.derive_public_hash_batch(&secrets).await;
    let elapsed = start.elapsed();
    println!(
        "  {:<30} {:>8} tokens  {:.3}s  {:>12.0} T/s",
        "Token derivation (n=10000)",
        secrets.len(),
        elapsed.as_secs_f64(),
        secrets.len() as f64 / elapsed.as_secs_f64(),
    );
    // Verify first
    let expected = hex::encode(Sha256::digest(secrets[0].as_bytes()));
    assert_eq!(derivations[0].public_hash, expected);

    // wgpu backend (if available)
    #[cfg(feature = "wgpu-compute")]
    {
        println!("\n--- wgpu backend (GPU) ---");
        match compute::wgpu_compute::WgpuBackend::new() {
            Ok(gpu) => {
                // Standard preimage size (200 bytes)
                for size in [1000, 10000, 65536, 100000] {
                    let inputs = random_inputs(size, 200);
                    bench_backend(&format!("SHA256 200B (n={size})"), &gpu, &inputs).await;
                }

                // Token derivation size (64 bytes = one SHA256 block, minimal transfer)
                println!();
                for size in [1000, 10000, 65536] {
                    let inputs = random_inputs(size, 64);
                    bench_backend(&format!("SHA256 64B (n={size})"), &gpu, &inputs).await;
                }

                println!();
                bench_pow("PoW verify (n=10000, d=0)", &gpu, &preimages).await;

                println!();
                let start = Instant::now();
                let gpu_derivations = gpu.derive_public_hash_batch(&secrets).await;
                let elapsed = start.elapsed();
                println!(
                    "  {:<30} {:>8} tokens  {:.3}s  {:>12.0} T/s",
                    "Token derivation (n=10000)",
                    secrets.len(),
                    elapsed.as_secs_f64(),
                    secrets.len() as f64 / elapsed.as_secs_f64(),
                );
                assert_eq!(gpu_derivations[0].public_hash, expected);
            }
            Err(e) => {
                println!("  wgpu not available: {e}");
            }
        }
    }

    println!("\n{}", "=".repeat(80));
    println!("  Done");
    println!("{}\n", "=".repeat(80));
}
