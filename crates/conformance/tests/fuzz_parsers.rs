//! Parser fuzz suite. Runs a high-case proptest over completely
//! arbitrary byte strings (NOT constrained to the canonical wire
//! format) against every parser. The contract is total:
//!   parse(any-string) → Ok(_) | Err(_), never panic, OOM, or
//!   silently consume input it shouldn't.
//!
//! Default 4096 cases per test (~30s wall clock). For a deeper run:
//!   PROPTEST_CASES=1000000 cargo test --release --test fuzz_parsers
//! That's ~10 minutes of fuzzing across all parsers.
//!
//! Stable-Rust friendly; no nightly cargo-fuzz toolchain required.

use proptest::prelude::*;

use webycash_asset_rgb::{PublicCollectible, PublicFungible, SecretCollectible, SecretFungible};
use webycash_asset_voucher::{PublicVoucher, SecretVoucher};
use webycash_asset_webcash::{PublicWebcash, SecretWebcash};

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        // No need to shrink — we only care about whether ANY input
        // panics. Shrinking on a fuzz suite costs minutes per failure.
        max_shrink_iters: 0,
        ..ProptestConfig::default()
    })]

    /// Webcash secret parser is total over arbitrary strings.
    #[test]
    fn fuzz_secret_webcash(s: String) {
        let _ = SecretWebcash::parse(&s);
    }

    #[test]
    fn fuzz_public_webcash(s: String) {
        let _ = PublicWebcash::parse(&s);
    }

    #[test]
    fn fuzz_secret_fungible(s: String) {
        let _ = SecretFungible::parse(&s);
    }

    #[test]
    fn fuzz_public_fungible(s: String) {
        let _ = PublicFungible::parse(&s);
    }

    #[test]
    fn fuzz_secret_collectible(s: String) {
        let _ = SecretCollectible::parse(&s);
    }

    #[test]
    fn fuzz_public_collectible(s: String) {
        let _ = PublicCollectible::parse(&s);
    }

    #[test]
    fn fuzz_secret_voucher(s: String) {
        let _ = SecretVoucher::parse(&s);
    }

    #[test]
    fn fuzz_public_voucher(s: String) {
        let _ = PublicVoucher::parse(&s);
    }

    /// Bias the input toward `:secret:` / `:public:` markers so the
    /// parser actually exercises its post-marker code paths. Catches
    /// the bug class where a parser panics on an unexpected character
    /// AFTER successfully consuming the marker.
    #[test]
    fn fuzz_secret_webcash_with_marker(
        prefix in "[a-zA-Z0-9.:e]{0,32}",
        suffix: String,
    ) {
        let s = format!("{prefix}:secret:{suffix}");
        let _ = SecretWebcash::parse(&s);
    }

    #[test]
    fn fuzz_secret_collectible_with_marker(
        suffix: String,
    ) {
        let s = format!("secret:{suffix}");
        let _ = SecretCollectible::parse(&s);
    }
}
