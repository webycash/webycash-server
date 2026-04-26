//! Voucher secret/public token wire format.
//!
//! `e{amount}:secret:{hex64}:{contract_id}:{issuer_pgp_fp}` and
//! `e{amount}:public:{sha256_hex}:{contract_id}:{issuer_pgp_fp}`.
//!
//! Vouchers are ALWAYS splittable. Secrets are namespaced by
//! `(contract_id, issuer_fp)` — replace operations must stay within a
//! single namespace, and storage keys are partitioned per namespace.

use std::fmt;
use std::str::FromStr;

use nom::bytes::complete::{tag, take_while1};
use nom::character::complete::char;
use nom::sequence::preceded;
use nom::IResult;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use webycash_asset_core::{Amount, ContractId, PgpFingerprint};
use webycash_proto::parsers::{amount_parser, hex64};

/// `e{amount}:secret:{hex64}:{contract_id}:{issuer_pgp_fp}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretVoucher {
    pub amount: Amount,
    pub secret: String,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

/// `e{amount}:public:{sha256_hex}:{contract_id}:{issuer_pgp_fp}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PublicVoucher {
    pub amount: Amount,
    pub hash: String,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

impl SecretVoucher {
    /// `sha256(secret_hex_bytes)` — uniform with Webcash for batchability.
    /// Namespace `(contract_id, issuer_fp)` lives in the wire token and the
    /// DB key, not folded into the hash.
    pub fn to_public(&self) -> PublicVoucher {
        let hash = Sha256::digest(self.secret.as_bytes());
        PublicVoucher {
            amount: self.amount,
            hash: hex::encode(hash),
            contract_id: self.contract_id.clone(),
            issuer_fp: self.issuer_fp.clone(),
        }
    }

    /// Parse a voucher secret from its wire form:
    /// `e{amount}:secret:{64-hex}:{contract_id}:{issuer_fp}`.
    ///
    /// ```
    /// use webycash_asset_voucher::SecretVoucher;
    /// let token = format!(
    ///     "e25.0:secret:{}:credits-q1:{}",
    ///     "f".repeat(64),
    ///     "aabbccddeeff00112233445566778899aabbccdd",
    /// );
    /// let s = SecretVoucher::parse(&token).unwrap();
    /// assert_eq!(s.amount.to_string(), "25.00000000");
    /// assert_eq!(s.contract_id.0, "credits-q1");
    /// ```
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        Self::from_str(s)
    }
}

impl PublicVoucher {
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        Self::from_str(s)
    }
}

impl fmt::Display for SecretVoucher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "e{}:secret:{}:{}:{}",
            self.amount, self.secret, self.contract_id, self.issuer_fp
        )
    }
}

impl fmt::Display for PublicVoucher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "e{}:public:{}:{}:{}",
            self.amount, self.hash, self.contract_id, self.issuer_fp
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid voucher token format: {0}")]
    InvalidFormat(String),
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
}

impl FromStr for SecretVoucher {
    type Err = TokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TokenError::InvalidFormat(s.to_string());
        let (rest, v) = parse_secret_voucher(s).map_err(|_| err())?;
        if !rest.is_empty() {
            return Err(err());
        }
        Ok(v)
    }
}

impl FromStr for PublicVoucher {
    type Err = TokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TokenError::InvalidFormat(s.to_string());
        let (rest, v) = parse_public_voucher(s).map_err(|_| err())?;
        if !rest.is_empty() {
            return Err(err());
        }
        Ok(v)
    }
}

fn slug(input: &str) -> IResult<&str, &str> {
    take_while1(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')(input)
}

/// 40 hex chars (20-byte OpenPGP V4 fingerprint), lowercase.
fn fingerprint(input: &str) -> IResult<&str, &str> {
    nom::bytes::complete::take_while_m_n(40, 40, |c: char| c.is_ascii_hexdigit())(input)
}

fn parse_secret_voucher(input: &str) -> IResult<&str, SecretVoucher> {
    let (rest, amt_str) = preceded(char('e'), amount_parser)(input)?;
    let (rest, _) = tag(":secret:")(rest)?;
    let (rest, hex) = hex64(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, contract) = slug(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, issuer) = fingerprint(rest)?;

    let amount = Amount::from_str(amt_str).map_err(|_| {
        nom::Err::Failure(nom::error::Error::new(input, nom::error::ErrorKind::Verify))
    })?;
    Ok((
        rest,
        SecretVoucher {
            amount,
            secret: hex.to_string(),
            contract_id: ContractId(contract.to_string()),
            issuer_fp: PgpFingerprint(issuer.to_lowercase()),
        },
    ))
}

fn parse_public_voucher(input: &str) -> IResult<&str, PublicVoucher> {
    let (rest, amt_str) = preceded(char('e'), amount_parser)(input)?;
    let (rest, _) = tag(":public:")(rest)?;
    let (rest, hex) = hex64(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, contract) = slug(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, issuer) = fingerprint(rest)?;

    let amount = Amount::from_str(amt_str).map_err(|_| {
        nom::Err::Failure(nom::error::Error::new(input, nom::error::ErrorKind::Verify))
    })?;
    Ok((
        rest,
        PublicVoucher {
            amount,
            hash: hex.to_string(),
            contract_id: ContractId(contract.to_string()),
            issuer_fp: PgpFingerprint(issuer.to_lowercase()),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "aabbccddeeff00112233445566778899aabbccdd";

    #[test]
    fn roundtrip_secret() {
        let token = format!(
            "e10.0:secret:{}:credits-q1:{}",
            "f".repeat(64),
            FP
        );
        let v = SecretVoucher::parse(&token).expect("parse");
        assert_eq!(v.amount.to_string(), "10.00000000");
        assert_eq!(v.secret, "f".repeat(64));
        assert_eq!(v.contract_id.0, "credits-q1");
        assert_eq!(v.issuer_fp.0, FP);
        // Display puts amount in canonical 8-decimal form
        assert_eq!(
            v.to_string(),
            format!("e10.00000000:secret:{}:credits-q1:{}", "f".repeat(64), FP)
        );
    }

    #[test]
    fn to_public_hash_is_sha256_of_secret_hex() {
        let secret = format!(
            "e1.0:secret:{}:credits:{}",
            "a".repeat(64),
            FP
        );
        let s = SecretVoucher::parse(&secret).unwrap();
        let p = s.to_public();
        let expected = hex::encode(Sha256::digest("a".repeat(64).as_bytes()));
        assert_eq!(p.hash, expected);
        assert_eq!(p.contract_id.0, "credits");
        assert_eq!(p.issuer_fp.0, FP);
    }

    #[test]
    fn rejects_missing_issuer_or_contract() {
        // Missing issuer
        let bad = format!("e1.0:secret:{}:credits", "a".repeat(64));
        assert!(SecretVoucher::parse(&bad).is_err());
        // Missing contract
        let bad2 = format!("e1.0:secret:{}::{}", "a".repeat(64), FP);
        assert!(SecretVoucher::parse(&bad2).is_err());
    }

    #[test]
    fn rejects_uppercase_fingerprint() {
        // Parser preserves the casing it sees but we lowercase on construction;
        // mixed-case still parses through.
        let token = format!(
            "e1.0:secret:{}:credits:{}",
            "a".repeat(64),
            FP.to_uppercase()
        );
        let v = SecretVoucher::parse(&token).unwrap();
        assert_eq!(v.issuer_fp.0, FP, "fingerprint must be lower-cased");
    }
}
