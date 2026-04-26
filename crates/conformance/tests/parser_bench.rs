//! Parser throughput micro-benchmark. NOT a criterion bench — those
//! pull tens of seconds of compile time and statistical analysis we
//! don't need here. Hand-rolled wall-clock loop: parse 100k canonical
//! tokens per parser and print ns/op + ops/sec.
//!
//! Marked `#[ignore]` so it doesn't run under `cargo test` by default;
//! run explicitly with:
//!
//!   cargo test --release --test parser_bench -- --ignored --nocapture
//!
//! Used as a regression-detection contract for the bench-parity goal
//! (≥12.7k TPS Webcash, ≥5k TPS RGB/Voucher) — these microbenches
//! aren't end-to-end TPS but they catch a 10x slowdown in the parser
//! layer without needing a running server.

use std::time::Instant;

use webycash_asset_rgb::{SecretCollectible, SecretFungible};
use webycash_asset_voucher::SecretVoucher;
use webycash_asset_webcash::SecretWebcash;

const ITERATIONS: usize = 100_000;
const FP: &str = "aabbccddeeff00112233445566778899aabbccdd";

fn report(name: &str, elapsed_ns: u128) {
    let ns_per_op = elapsed_ns / ITERATIONS as u128;
    let ops_per_sec = if ns_per_op == 0 {
        u128::MAX
    } else {
        1_000_000_000 / ns_per_op
    };
    println!(
        "{name:>32}: {ns_per_op:>6} ns/op  ({ops_per_sec:>9} ops/s, {ITERATIONS} iters)",
    );
}

#[test]
#[ignore = "throughput micro-bench; run with --ignored --nocapture"]
fn bench_parsers_release() {
    let webcash_token = format!(
        "e1.0:secret:{}",
        "a".repeat(64),
    );
    let rgb_fungible_token = format!(
        "e10.0:secret:{}:rgb20-usdc:{FP}",
        "b".repeat(64),
    );
    let rgb_collectible_token = format!(
        "secret:{}:rgb21-art:{FP}",
        "c".repeat(64),
    );
    let voucher_token = format!(
        "e25.0:secret:{}:credits-q1:{FP}",
        "d".repeat(64),
    );

    let bench = |name: &str, parse: &dyn Fn() -> bool| {
        let t0 = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = std::hint::black_box(parse());
        }
        let elapsed_ns = t0.elapsed().as_nanos();
        report(name, elapsed_ns);
    };

    println!();
    println!("=== Parser throughput ({ITERATIONS} iters, hand-rolled wall-clock) ===");
    bench("SecretWebcash::parse", &|| {
        SecretWebcash::parse(&webcash_token).is_ok()
    });
    bench("SecretFungible::parse", &|| {
        SecretFungible::parse(&rgb_fungible_token).is_ok()
    });
    bench("SecretCollectible::parse", &|| {
        SecretCollectible::parse(&rgb_collectible_token).is_ok()
    });
    bench("SecretVoucher::parse", &|| {
        SecretVoucher::parse(&voucher_token).is_ok()
    });

    bench("SecretWebcash::to_public", &|| {
        let s = SecretWebcash::parse(&webcash_token).unwrap();
        !s.to_public().hash.is_empty()
    });
    bench("SecretVoucher::to_public", &|| {
        let s = SecretVoucher::parse(&voucher_token).unwrap();
        !s.to_public().hash.is_empty()
    });
}
