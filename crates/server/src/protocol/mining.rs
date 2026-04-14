use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Amount;

/// Count leading zero bits in a 32-byte SHA256 hash.
pub fn leading_zero_bits(hash: &[u8]) -> u32 {
    let mut count = 0u32;
    for byte in hash {
        if *byte == 0 {
            count += 8;
        } else {
            count += byte.leading_zeros();
            break;
        }
    }
    count
}

/// Verify that SHA256(preimage) has at least `difficulty_bits` leading zero bits.
pub fn verify_pow(preimage: &str, difficulty_bits: u32) -> bool {
    let hash = Sha256::digest(preimage.as_bytes());
    leading_zero_bits(&hash) >= difficulty_bits
}

/// Parsed mining preimage. The preimage JSON encodes:
/// ```json
/// {
///   "webcash": ["e{amount}:secret:{hex}"],
///   "subsidy": ["e{amount}:secret:{hex}"],
///   "timestamp": unix_seconds,
///   "difficulty": target_bits
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningPreimage {
    pub webcash: Vec<String>,
    pub subsidy: Vec<String>,
    pub timestamp: u64,
    pub difficulty: u32,
}

/// Result of mining target query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetInfo {
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub mining_amount: Amount,
    pub mining_subsidy_amount: Amount,
    pub ratio: f64,
}

/// Mining state tracked by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningState {
    pub difficulty_target_bits: u32,
    pub epoch: u32,
    pub total_circulation_wats: i64,
    pub mining_reports_count: u64,
    pub mining_amount_wats: i64,
    pub subsidy_amount_wats: i64,
    pub last_adjustment_at: chrono::DateTime<chrono::Utc>,
    pub aggregate_work: f64,
}

impl MiningState {
    pub fn initial(difficulty: u32, mining_amount_wats: i64, subsidy_amount_wats: i64) -> Self {
        Self {
            difficulty_target_bits: difficulty,
            epoch: 0,
            total_circulation_wats: 0,
            mining_reports_count: 0,
            mining_amount_wats,
            subsidy_amount_wats,
            last_adjustment_at: chrono::Utc::now(),
            aggregate_work: 0.0,
        }
    }

    pub fn to_target_info(&self) -> TargetInfo {
        let total = self.mining_amount_wats + self.subsidy_amount_wats;
        let ratio = if total > 0 {
            self.mining_amount_wats as f64 / total as f64
        } else {
            1.0
        };
        TargetInfo {
            difficulty_target_bits: self.difficulty_target_bits,
            epoch: self.epoch,
            mining_amount: Amount::from_wats(self.mining_amount_wats),
            mining_subsidy_amount: Amount::from_wats(self.subsidy_amount_wats),
            ratio,
        }
    }
}

/// Difficulty adjustment (production mode only).
/// Returns the new difficulty after evaluating the epoch.
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
        hash[0] = 0x01; // 7 leading zeros
        assert_eq!(leading_zero_bits(&hash), 7);
    }

    #[test]
    fn leading_zeros_two_bytes() {
        let mut hash = [0u8; 32];
        hash[0] = 0x00;
        hash[1] = 0x0F; // 8 + 4 = 12 leading zeros
        assert_eq!(leading_zero_bits(&hash), 12);
    }

    #[test]
    fn leading_zeros_16_bits() {
        let mut hash = [0u8; 32];
        hash[0] = 0x00;
        hash[1] = 0x00;
        hash[2] = 0xFF;
        assert_eq!(leading_zero_bits(&hash), 16);
    }

    #[test]
    fn verify_pow_trivial() {
        // Difficulty 0 means any hash passes
        assert!(verify_pow("anything", 0));
    }

    #[test]
    fn difficulty_adjustment_too_fast() {
        let new = adjust_difficulty(16, 500, 1000, 100, 100);
        assert_eq!(new, 17); // increased
    }

    #[test]
    fn difficulty_adjustment_too_slow() {
        let new = adjust_difficulty(16, 2000, 1000, 50, 100);
        assert_eq!(new, 15); // decreased
    }

    #[test]
    fn difficulty_adjustment_stable() {
        let new = adjust_difficulty(16, 1000, 1000, 100, 100);
        // time_ratio = 1.0, report_ratio = 1.0, both >= 1.0 triggers increase
        // Actually time_ratio <= 1.0 AND report_ratio >= 1.0 → increase
        assert_eq!(new, 17);
    }

    #[test]
    fn difficulty_floor() {
        let new = adjust_difficulty(1, 99999, 1000, 1, 100);
        assert_eq!(new, 1); // can't go below 1
    }
}
