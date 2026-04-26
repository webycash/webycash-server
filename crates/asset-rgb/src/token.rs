//! RGB secret/public token wire formats.
//!
//! Two flavors share this module:
//!
//! - **RGB20 fungible** (splittable):
//!   `e{amount}:secret:{hex64}:{contract_id}:{issuer_pgp_fp}`
//!   `e{amount}:public:{sha256_hex}:{contract_id}:{issuer_pgp_fp}`
//!
//! - **RGB21 NFT** (non-splittable, no amount segment):
//!   `secret:{hex64}:{contract_id}:{issuer_pgp_fp}`
//!   `public:{sha256_hex}:{contract_id}:{issuer_pgp_fp}`
//!
//! Both are issuer-namespaced. AluVM transition validation lives in
//! `webycash-aluvm-runtime`; this module handles wire format only.

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

// ─────────────────────────────────────────────────────────────────────────────
// RGB20 fungible (splittable)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretFungible {
    pub amount: Amount,
    pub secret: String,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PublicFungible {
    pub amount: Amount,
    pub hash: String,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

impl SecretFungible {
    pub fn to_public(&self) -> PublicFungible {
        let hash = Sha256::digest(self.secret.as_bytes());
        PublicFungible {
            amount: self.amount,
            hash: hex::encode(hash),
            contract_id: self.contract_id.clone(),
            issuer_fp: self.issuer_fp.clone(),
        }
    }

    /// Parse an RGB20 fungible secret from its wire form:
    /// `e{amount}:secret:{64-hex}:{contract_id}:{issuer_fp}`.
    ///
    /// ```
    /// use webycash_asset_rgb::SecretFungible;
    /// let token = format!(
    ///     "e10.0:secret:{}:rgb20-usdc:{}",
    ///     "a".repeat(64),
    ///     "aabbccddeeff00112233445566778899aabbccdd",
    /// );
    /// let s = SecretFungible::parse(&token).unwrap();
    /// assert_eq!(s.amount.to_string(), "10.00000000");
    /// assert_eq!(s.contract_id.0, "rgb20-usdc");
    /// ```
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        Self::from_str(s)
    }
}

impl PublicFungible {
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        Self::from_str(s)
    }
}

impl fmt::Display for SecretFungible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "e{}:secret:{}:{}:{}",
            self.amount, self.secret, self.contract_id, self.issuer_fp
        )
    }
}

impl fmt::Display for PublicFungible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "e{}:public:{}:{}:{}",
            self.amount, self.hash, self.contract_id, self.issuer_fp
        )
    }
}

impl FromStr for SecretFungible {
    type Err = TokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TokenError::InvalidFormat(s.to_string());
        let (rest, v) = parse_secret_fungible(s).map_err(|_| err())?;
        if !rest.is_empty() {
            return Err(err());
        }
        Ok(v)
    }
}

impl FromStr for PublicFungible {
    type Err = TokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TokenError::InvalidFormat(s.to_string());
        let (rest, v) = parse_public_fungible(s).map_err(|_| err())?;
        if !rest.is_empty() {
            return Err(err());
        }
        Ok(v)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RGB21 NFT (non-splittable; no amount segment)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretCollectible {
    pub secret: String,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PublicCollectible {
    pub hash: String,
    pub contract_id: ContractId,
    pub issuer_fp: PgpFingerprint,
}

impl SecretCollectible {
    pub fn to_public(&self) -> PublicCollectible {
        let hash = Sha256::digest(self.secret.as_bytes());
        PublicCollectible {
            hash: hex::encode(hash),
            contract_id: self.contract_id.clone(),
            issuer_fp: self.issuer_fp.clone(),
        }
    }

    /// Parse an RGB21 collectible secret. Note: NO leading `e{amount}:`
    /// segment — collectibles are non-splittable and don't carry an
    /// amount on the wire.
    ///
    /// ```
    /// use webycash_asset_rgb::SecretCollectible;
    /// let token = format!(
    ///     "secret:{}:rgb21-art-1:{}",
    ///     "a".repeat(64),
    ///     "aabbccddeeff00112233445566778899aabbccdd",
    /// );
    /// let s = SecretCollectible::parse(&token).unwrap();
    /// assert_eq!(s.contract_id.0, "rgb21-art-1");
    /// // Adding a stray amount segment must fail (catches the
    /// // wrong-flavor mistake at parse time).
    /// let with_amount = format!("e1.0:{token}");
    /// assert!(SecretCollectible::parse(&with_amount).is_err());
    /// ```
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        Self::from_str(s)
    }
}

impl PublicCollectible {
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        Self::from_str(s)
    }
}

impl fmt::Display for SecretCollectible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "secret:{}:{}:{}",
            self.secret, self.contract_id, self.issuer_fp
        )
    }
}

impl fmt::Display for PublicCollectible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "public:{}:{}:{}",
            self.hash, self.contract_id, self.issuer_fp
        )
    }
}

impl FromStr for SecretCollectible {
    type Err = TokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TokenError::InvalidFormat(s.to_string());
        let (rest, v) = parse_secret_collectible(s).map_err(|_| err())?;
        if !rest.is_empty() {
            return Err(err());
        }
        Ok(v)
    }
}

impl FromStr for PublicCollectible {
    type Err = TokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TokenError::InvalidFormat(s.to_string());
        let (rest, v) = parse_public_collectible(s).map_err(|_| err())?;
        if !rest.is_empty() {
            return Err(err());
        }
        Ok(v)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors + parsers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid RGB token format: {0}")]
    InvalidFormat(String),
}

fn slug(input: &str) -> IResult<&str, &str> {
    take_while1(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')(input)
}

fn fingerprint(input: &str) -> IResult<&str, &str> {
    nom::bytes::complete::take_while_m_n(40, 40, |c: char| c.is_ascii_hexdigit())(input)
}

fn parse_secret_fungible(input: &str) -> IResult<&str, SecretFungible> {
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
        SecretFungible {
            amount,
            secret: hex.to_string(),
            contract_id: ContractId(contract.to_string()),
            issuer_fp: PgpFingerprint(issuer.to_lowercase()),
        },
    ))
}

fn parse_public_fungible(input: &str) -> IResult<&str, PublicFungible> {
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
        PublicFungible {
            amount,
            hash: hex.to_string(),
            contract_id: ContractId(contract.to_string()),
            issuer_fp: PgpFingerprint(issuer.to_lowercase()),
        },
    ))
}

fn parse_secret_collectible(input: &str) -> IResult<&str, SecretCollectible> {
    let (rest, _) = tag("secret:")(input)?;
    let (rest, hex) = hex64(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, contract) = slug(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, issuer) = fingerprint(rest)?;
    Ok((
        rest,
        SecretCollectible {
            secret: hex.to_string(),
            contract_id: ContractId(contract.to_string()),
            issuer_fp: PgpFingerprint(issuer.to_lowercase()),
        },
    ))
}

fn parse_public_collectible(input: &str) -> IResult<&str, PublicCollectible> {
    let (rest, _) = tag("public:")(input)?;
    let (rest, hex) = hex64(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, contract) = slug(rest)?;
    let (rest, _) = tag(":")(rest)?;
    let (rest, issuer) = fingerprint(rest)?;
    Ok((
        rest,
        PublicCollectible {
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
    fn rgb20_fungible_roundtrip() {
        let token = format!("e10.0:secret:{}:rgb20-usdc:{}", "a".repeat(64), FP);
        let s = SecretFungible::parse(&token).unwrap();
        assert_eq!(s.amount.to_string(), "10.00000000");
        assert_eq!(s.contract_id.0, "rgb20-usdc");
        assert_eq!(s.issuer_fp.0, FP);
    }

    #[test]
    fn rgb21_collectible_no_amount_segment() {
        let token = format!("secret:{}:rgb21-art-1:{}", "a".repeat(64), FP);
        let s = SecretCollectible::parse(&token).unwrap();
        assert_eq!(s.contract_id.0, "rgb21-art-1");
        assert_eq!(s.issuer_fp.0, FP);
    }

    #[test]
    fn collectible_rejects_amount_prefix() {
        let token = format!("e1.0:secret:{}:rgb21-art-1:{}", "a".repeat(64), FP);
        // Should NOT parse as collectible (has amount).
        assert!(SecretCollectible::parse(&token).is_err());
    }

    #[test]
    fn fungible_rejects_no_amount() {
        let token = format!("secret:{}:rgb20-usdc:{}", "a".repeat(64), FP);
        // Should NOT parse as fungible (lacks amount).
        assert!(SecretFungible::parse(&token).is_err());
    }

    #[test]
    fn fungible_to_public_hash() {
        let token = format!("e1.0:secret:{}:rgb20:{}", "a".repeat(64), FP);
        let s = SecretFungible::parse(&token).unwrap();
        let p = s.to_public();
        let expected = hex::encode(Sha256::digest("a".repeat(64).as_bytes()));
        assert_eq!(p.hash, expected);
    }

    #[test]
    fn collectible_to_public_hash() {
        let token = format!("secret:{}:rgb21:{}", "b".repeat(64), FP);
        let s = SecretCollectible::parse(&token).unwrap();
        let p = s.to_public();
        let expected = hex::encode(Sha256::digest("b".repeat(64).as_bytes()));
        assert_eq!(p.hash, expected);
    }
}
