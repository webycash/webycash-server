use std::fmt;
use std::str::FromStr;

use nom::bytes::complete::tag;
use nom::character::complete::digit1;
use nom::combinator::{opt, recognize};
use nom::sequence::tuple;
use nom::IResult;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Fixed-point amount with 8 decimal places (wats).
/// 1.00000000 webcash = 100_000_000 wats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount {
    pub wats: i64,
}

const DECIMALS: u32 = 8;
const SCALE: i64 = 10i64.pow(DECIMALS);

impl Amount {
    pub const ZERO: Amount = Amount { wats: 0 };

    pub fn from_wats(wats: i64) -> Self {
        Amount { wats }
    }

    pub fn is_zero(&self) -> bool {
        self.wats == 0
    }

    pub fn is_positive(&self) -> bool {
        self.wats > 0
    }

    pub fn checked_add(self, other: Amount) -> Option<Amount> {
        self.wats.checked_add(other.wats).map(|w| Amount { wats: w })
    }

    pub fn checked_sub(self, other: Amount) -> Option<Amount> {
        self.wats.checked_sub(other.wats).map(|w| Amount { wats: w })
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let abs = self.wats.unsigned_abs();
        let whole = abs / SCALE as u64;
        let frac = abs % SCALE as u64;
        if self.wats < 0 {
            write!(f, "-{}.{:08}", whole, frac)
        } else {
            write!(f, "{}.{:08}", whole, frac)
        }
    }
}

impl FromStr for Amount {
    type Err = AmountError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_amount_str(s)
    }
}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Amount::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Panics on overflow. Use checked_add() for untrusted input.
impl std::ops::Add for Amount {
    type Output = Amount;
    fn add(self, rhs: Amount) -> Amount {
        self.checked_add(rhs).expect("amount overflow in addition")
    }
}

/// Panics on underflow. Use checked_sub() for untrusted input.
impl std::ops::Sub for Amount {
    type Output = Amount;
    fn sub(self, rhs: Amount) -> Amount {
        self.checked_sub(rhs).expect("amount underflow in subtraction")
    }
}

impl std::iter::Sum for Amount {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Amount::ZERO, |a, b| {
            a.checked_add(b).expect("amount overflow in sum")
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AmountError {
    #[error("invalid amount format: {0}")]
    InvalidFormat(String),
    #[error("amount overflow")]
    Overflow,
    #[error("too many decimal places (max 8)")]
    TooManyDecimals,
}

fn parse_amount_str(input: &str) -> Result<Amount, AmountError> {
    let negative = input.starts_with('-');
    let s = if negative { &input[1..] } else { input };

    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        1 => {
            let whole: i64 = parts[0]
                .parse()
                .map_err(|_| AmountError::InvalidFormat(input.to_string()))?;
            let wats = whole
                .checked_mul(SCALE)
                .ok_or(AmountError::Overflow)?;
            Ok(Amount {
                wats: if negative { -wats } else { wats },
            })
        }
        2 => {
            let whole: i64 = parts[0]
                .parse()
                .map_err(|_| AmountError::InvalidFormat(input.to_string()))?;
            let frac_str = parts[1];
            if frac_str.len() > DECIMALS as usize {
                return Err(AmountError::TooManyDecimals);
            }
            let padded = format!("{:0<8}", frac_str);
            let frac: i64 = padded
                .parse()
                .map_err(|_| AmountError::InvalidFormat(input.to_string()))?;
            let wats = whole
                .checked_mul(SCALE)
                .and_then(|w| w.checked_add(frac))
                .ok_or(AmountError::Overflow)?;
            Ok(Amount {
                wats: if negative { -wats } else { wats },
            })
        }
        _ => Err(AmountError::InvalidFormat(input.to_string())),
    }
}

/// nom parser for amount strings (used by token parsers).
pub fn amount_parser(input: &str) -> IResult<&str, &str> {
    recognize(tuple((digit1, opt(tuple((tag("."), digit1))))))(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_whole() {
        let a = Amount::from_str("200").unwrap();
        assert_eq!(a.wats, 20_000_000_000);
        assert_eq!(a.to_string(), "200.00000000");
    }

    #[test]
    fn parse_decimal() {
        let a = Amount::from_str("1.50000000").unwrap();
        assert_eq!(a.wats, 150_000_000);
    }

    #[test]
    fn parse_short_decimal() {
        let a = Amount::from_str("0.1").unwrap();
        assert_eq!(a.wats, 10_000_000);
    }

    #[test]
    fn zero() {
        let a = Amount::from_str("0").unwrap();
        assert!(a.is_zero());
    }

    #[test]
    fn arithmetic() {
        let a = Amount::from_str("10.00000000").unwrap();
        let b = Amount::from_str("3.50000000").unwrap();
        assert_eq!((a - b).to_string(), "6.50000000");
        assert_eq!((a + b).to_string(), "13.50000000");
    }

    #[test]
    fn sum() {
        let amounts = vec![
            Amount::from_str("1.00000000").unwrap(),
            Amount::from_str("2.00000000").unwrap(),
            Amount::from_str("3.00000000").unwrap(),
        ];
        let total: Amount = amounts.into_iter().sum();
        assert_eq!(total.to_string(), "6.00000000");
    }

    #[test]
    fn too_many_decimals() {
        assert!(Amount::from_str("1.123456789").is_err());
    }

    #[test]
    fn roundtrip() {
        let original = "42.12345678";
        let a = Amount::from_str(original).unwrap();
        assert_eq!(a.to_string(), original);
    }
}
