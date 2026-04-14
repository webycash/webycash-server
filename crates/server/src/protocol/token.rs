use std::fmt;
use std::str::FromStr;

use nom::bytes::complete::{tag, take_while_m_n};
use nom::character::complete::char;
use nom::sequence::{preceded, separated_pair};
use nom::IResult;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::amount::{amount_parser, Amount};

/// A secret webcash token: `e{amount}:secret:{64-hex-chars}`.
#[derive(Debug, Clone)]
pub struct SecretWebcash {
    pub amount: Amount,
    pub secret: String,
}

/// A public webcash token: `e{amount}:public:{64-hex-chars}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublicWebcash {
    pub amount: Amount,
    pub hash: String,
}

impl SecretWebcash {
    /// Convert to PublicWebcash by hashing the SECRET HEX STRING only.
    /// CRITICAL: Must match production webcash.org behavior.
    /// Python: hashlib.sha256(bytes(str(secret_value), "ascii")).hexdigest()
    /// We hash the 64-char hex secret, NOT the full "e{amount}:secret:{hex}" string.
    pub fn to_public(&self) -> PublicWebcash {
        let hash = Sha256::digest(self.secret.as_bytes());
        PublicWebcash {
            amount: self.amount,
            hash: hex::encode(hash),
        }
    }
}

impl fmt::Display for SecretWebcash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}:secret:{}", self.amount, self.secret)
    }
}

impl fmt::Display for PublicWebcash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}:public:{}", self.amount, self.hash)
    }
}

impl FromStr for SecretWebcash {
    type Err = TokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_secret_webcash(s)
            .map(|(_, wc)| wc)
            .map_err(|_| TokenError::InvalidFormat(s.to_string()))
    }
}

impl FromStr for PublicWebcash {
    type Err = TokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_public_webcash(s)
            .map(|(_, wc)| wc)
            .map_err(|_| TokenError::InvalidFormat(s.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid token format: {0}")]
    InvalidFormat(String),
}

// --- nom parsers ---

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn hex64(input: &str) -> IResult<&str, &str> {
    take_while_m_n(64, 64, is_hex)(input)
}

fn parse_secret_webcash(input: &str) -> IResult<&str, SecretWebcash> {
    let (rest, (amt_str, secret)) = preceded(
        char('e'),
        separated_pair(amount_parser, tag(":secret:"), hex64),
    )(input)?;

    let amount =
        Amount::from_str(amt_str).map_err(|_| nom::Err::Failure(nom::error::Error::new(input, nom::error::ErrorKind::Verify)))?;

    Ok((
        rest,
        SecretWebcash {
            amount,
            secret: secret.to_string(),
        },
    ))
}

fn parse_public_webcash(input: &str) -> IResult<&str, PublicWebcash> {
    let (rest, (amt_str, hash)) = preceded(
        char('e'),
        separated_pair(amount_parser, tag(":public:"), hex64),
    )(input)?;

    let amount =
        Amount::from_str(amt_str).map_err(|_| nom::Err::Failure(nom::error::Error::new(input, nom::error::ErrorKind::Verify)))?;

    Ok((
        rest,
        PublicWebcash {
            amount,
            hash: hash.to_string(),
        },
    ))
}

/// Parse a secret webcash string, returning the token and any remaining input.
pub fn parse_secret(input: &str) -> IResult<&str, SecretWebcash> {
    parse_secret_webcash(input)
}

/// Parse a public webcash string, returning the token and any remaining input.
pub fn parse_public(input: &str) -> IResult<&str, PublicWebcash> {
    parse_public_webcash(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "e200.00000000:secret:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    #[test]
    fn parse_secret_token() {
        let wc = SecretWebcash::from_str(TEST_SECRET).unwrap();
        assert_eq!(wc.amount.to_string(), "200.00000000");
        assert_eq!(wc.secret.len(), 64);
    }

    #[test]
    fn roundtrip_secret() {
        let wc = SecretWebcash::from_str(TEST_SECRET).unwrap();
        assert_eq!(wc.to_string(), TEST_SECRET);
    }

    #[test]
    fn secret_to_public() {
        let secret = SecretWebcash::from_str(TEST_SECRET).unwrap();
        let public = secret.to_public();
        assert_eq!(public.amount, secret.amount);
        assert_eq!(public.hash.len(), 64);
        // SHA256 of the SECRET HEX ONLY (matches production webcash.org / Python impl)
        let secret_hex = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let expected_hash = hex::encode(Sha256::digest(secret_hex.as_bytes()));
        assert_eq!(public.hash, expected_hash);
    }

    #[test]
    fn parse_public_token() {
        let s = "e1.00000000:public:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let wc = PublicWebcash::from_str(s).unwrap();
        assert_eq!(wc.amount.to_string(), "1.00000000");
        assert_eq!(wc.hash.len(), 64);
    }

    #[test]
    fn invalid_format() {
        assert!(SecretWebcash::from_str("not_webcash").is_err());
        assert!(SecretWebcash::from_str("e1:secret:short").is_err());
    }
}
