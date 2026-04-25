//! Mining configuration and difficulty algorithms.
//!
//! Three modes per flavor (operator-configurable via env):
//!   - `Disabled` — no mining endpoint exposed
//!   - `Fixed { difficulty }` — constant target
//!   - `Dynamic { initial, target_secs, reports_per_epoch }` — self-adjusting
//!
//! Defaults: Webcash unchanged (Dynamic prod, Fixed testnet); RGB and Voucher
//! default to Dynamic, infinite issuance cap.
//!
//! Pure functions (no IO): `leading_zero_bits`, `verify_pow`, `adjust_difficulty`.
//! Used by the `MintableAsset::verify_issuance` impls in the asset crates.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ─────────────────────────────────────────────────────────────────────────────
// Mining configuration (operator-driven via env / TOML)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum MiningMode {
    /// Mining endpoint disabled. RGB/Voucher operators may run issuer-private
    /// `/issue` only.
    Disabled,
    /// Constant difficulty. Used for testnet.
    Fixed { difficulty: u32 },
    /// Self-adjusting difficulty.
    Dynamic {
        initial: u32,
        target_secs: u64,
        reports_per_epoch: u32,
    },
}

impl MiningMode {
    /// Recommended Webcash production defaults.
    pub fn webcash_production() -> Self {
        MiningMode::Dynamic {
            initial: 24,
            target_secs: 1_000,
            reports_per_epoch: 1_000,
        }
    }
    /// Recommended Webcash testnet defaults — trivially mineable.
    pub fn webcash_testnet() -> Self {
        MiningMode::Fixed { difficulty: 16 }
    }
    /// Recommended RGB / Voucher defaults — same as Webcash production.
    pub fn issued_default() -> Self {
        Self::webcash_production()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningConfig {
    pub mode: MiningMode,
    /// Mining amount per report (in atomic units, 8-decimal "wats").
    /// Halved at each subsidy epoch boundary.
    pub mining_amount_wats: i64,
    /// Subsidy amount per report (in atomic units).
    pub subsidy_amount_wats: i64,
    /// Optional issuance cap. `None` = unlimited.
    pub max_issuance: Option<u128>,
    /// For RGB/Voucher: when true, `/issue` requires PoW IN ADDITION to
    /// the issuer signature. Webcash ignores this field.
    pub require_pow_for_issuance: bool,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            mode: MiningMode::webcash_production(),
            // 195.3125 webcash * 1e8 wats/webcash = 19_531_250_000 wats. Matches production.
            mining_amount_wats: 19_531_250_000,
            // 9.765625 webcash * 1e8 = 976_562_500 wats
            subsidy_amount_wats: 976_562_500,
            max_issuance: None,
            require_pow_for_issuance: false,
        }
    }
}

impl MiningConfig {
    /// Pull the active difficulty out of the configured mode. None when
    /// mining is disabled.
    pub fn current_difficulty(&self) -> Option<u32> {
        match &self.mode {
            MiningMode::Disabled => None,
            MiningMode::Fixed { difficulty } => Some(*difficulty),
            MiningMode::Dynamic { initial, .. } => Some(*initial),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PoW verification (pure functions)
// ─────────────────────────────────────────────────────────────────────────────

/// Count leading zero bits in a 32-byte SHA256 hash.
pub fn leading_zero_bits(hash: &[u8]) -> u32 {
    let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
    hash.get(full_zero_bytes as usize)
        .map_or(0, |b| b.leading_zeros())
        + full_zero_bytes * 8
}

/// Verify that SHA256(preimage) has at least `difficulty_bits` leading zero bits.
pub fn verify_pow(preimage: &str, difficulty_bits: u32) -> bool {
    let hash = Sha256::digest(preimage.as_bytes());
    leading_zero_bits(&hash) >= difficulty_bits
}

// ─────────────────────────────────────────────────────────────────────────────
// Dynamic difficulty adjustment
// ─────────────────────────────────────────────────────────────────────────────

/// Difficulty adjustment (production mode only). Returns the new difficulty
/// after evaluating the epoch. Clamped to ±2 bits per epoch (≤4x change).
pub fn adjust_difficulty(
    current_difficulty: u32,
    actual_time_secs: u64,
    target_time_secs: u64,
    actual_reports: u64,
    expected_reports: u64,
) -> u32 {
    let time_ratio = actual_time_secs as f64 / target_time_secs as f64;
    let report_ratio = actual_reports as f64 / expected_reports as f64;

    let new_diff = if time_ratio <= 1.0 && report_ratio >= 1.0 {
        // Mining too fast — increase difficulty
        current_difficulty.saturating_add(1)
    } else if time_ratio >= 1.0 && report_ratio <= 1.0 {
        // Mining too slow — decrease difficulty
        current_difficulty.saturating_sub(1).max(1)
    } else {
        current_difficulty
    };

    // Clamp: no more than 4x change per epoch
    let max = current_difficulty.saturating_add(2);
    let min = current_difficulty.saturating_sub(2).max(1);
    new_diff.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_zeros_all_zero() {
        let hash = [0u8; 32];
        assert_eq!(leading_zero_bits(&hash), 256);
    }

    #[test]
    fn leading_zeros_one_byte() {
        let mut hash = [0u8; 32];
        hash[0] = 0x01;
        assert_eq!(leading_zero_bits(&hash), 7);
    }

    #[test]
    fn leading_zeros_two_bytes() {
        let mut hash = [0u8; 32];
        hash[1] = 0x0F;
        assert_eq!(leading_zero_bits(&hash), 12);
    }

    #[test]
    fn verify_pow_trivial() {
        assert!(verify_pow("anything", 0));
    }

    #[test]
    fn difficulty_adjustment_too_fast() {
        assert_eq!(adjust_difficulty(16, 500, 1000, 100, 100), 17);
    }

    #[test]
    fn difficulty_adjustment_too_slow() {
        assert_eq!(adjust_difficulty(16, 2000, 1000, 50, 100), 15);
    }

    #[test]
    fn difficulty_floor() {
        assert_eq!(adjust_difficulty(1, 99999, 1000, 1, 100), 1);
    }

    #[test]
    fn current_difficulty_for_each_mode() {
        let cfg_disabled = MiningConfig {
            mode: MiningMode::Disabled,
            ..MiningConfig::default()
        };
        assert_eq!(cfg_disabled.current_difficulty(), None);

        let cfg_fixed = MiningConfig {
            mode: MiningMode::Fixed { difficulty: 8 },
            ..MiningConfig::default()
        };
        assert_eq!(cfg_fixed.current_difficulty(), Some(8));

        let cfg_dynamic = MiningConfig {
            mode: MiningMode::Dynamic {
                initial: 24,
                target_secs: 1000,
                reports_per_epoch: 1000,
            },
            ..MiningConfig::default()
        };
        assert_eq!(cfg_dynamic.current_difficulty(), Some(24));
    }
}
