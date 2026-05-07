//! Property tests for the wire-format parsers.
//!
//! For every asset flavor: any well-formed token round-trips
//! `Display → parse` to its original string AND yields a public hash
//! that matches `sha256(secret_hex_bytes)` regardless of namespace.

use proptest::prelude::*;
use sha2::{Digest, Sha256};

use webycash_server::asset_core::{Amount, ContractId, PgpFingerprint};
use webycash_server::asset_rgb::{
    PublicCollectible, PublicFungible, SecretCollectible, SecretFungible,
};
use webycash_server::asset_voucher::{PublicVoucher, SecretVoucher};
use webycash_server::asset_webcash::{PublicWebcash, SecretWebcash};

// ─── strategies ────────────────────────────────────────────────────────

/// 64 lowercase hex characters — the canonical secret encoding.
fn arb_secret_hex() -> impl Strategy<Value = String> {
    "[0-9a-f]{64}"
}

/// Any positive amount in wats up to half of i64::MAX (covers the full
/// production range plus subsidy outputs without addition overflow).
fn arb_amount() -> impl Strategy<Value = Amount> {
    (1i64..=i64::MAX / 2).prop_map(Amount::from_wats)
}

/// Voucher contract slug: alphanumeric + `-`/`_`, 1..=64 chars.
fn arb_contract_slug() -> impl Strategy<Value = ContractId> {
    "[a-zA-Z0-9_-]{1,64}".prop_map(ContractId)
}

/// PGP V4 fingerprint: lowercase hex, exactly 40 chars.
fn arb_fingerprint() -> impl Strategy<Value = PgpFingerprint> {
    "[0-9a-f]{40}".prop_map(PgpFingerprint)
}

// ─── webcash ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn webcash_secret_roundtrip(amount in arb_amount(), secret in arb_secret_hex()) {
        let s = SecretWebcash { amount, secret: secret.clone() };
        let printed = s.to_string();
        let parsed = SecretWebcash::parse(&printed).unwrap();
        prop_assert_eq!(parsed.amount, amount);
        prop_assert_eq!(&parsed.secret, &secret);
        prop_assert_eq!(parsed.to_string(), printed);
    }

    #[test]
    fn webcash_public_roundtrip(amount in arb_amount(), hash in arb_secret_hex()) {
        let p = PublicWebcash { amount, hash: hash.clone() };
        let printed = p.to_string();
        let parsed = PublicWebcash::parse(&printed).unwrap();
        prop_assert_eq!(parsed.amount, amount);
        prop_assert_eq!(parsed.hash, hash);
    }

    #[test]
    fn webcash_to_public_is_sha256_of_secret_hex(
        amount in arb_amount(),
        secret in arb_secret_hex(),
    ) {
        let s = SecretWebcash { amount, secret: secret.clone() };
        let p = s.to_public();
        let expected = hex::encode(Sha256::digest(secret.as_bytes()));
        prop_assert_eq!(p.hash, expected);
        prop_assert_eq!(p.amount, amount);
    }
}

// ─── rgb fungible (RGB20-shaped) ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn rgb_fungible_secret_roundtrip(
        amount in arb_amount(),
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let s = SecretFungible {
            amount,
            secret: secret.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let printed = s.to_string();
        let parsed = SecretFungible::parse(&printed).unwrap();
        prop_assert_eq!(parsed.amount, amount);
        prop_assert_eq!(parsed.secret, secret);
        prop_assert_eq!(parsed.contract_id, contract);
        prop_assert_eq!(parsed.issuer_fp, fp);
    }

    #[test]
    fn rgb_fungible_public_roundtrip(
        amount in arb_amount(),
        hash in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let p = PublicFungible {
            amount,
            hash: hash.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let printed = p.to_string();
        let parsed = PublicFungible::parse(&printed).unwrap();
        prop_assert_eq!(parsed.amount, amount);
        prop_assert_eq!(parsed.hash, hash);
        prop_assert_eq!(parsed.contract_id, contract);
        prop_assert_eq!(parsed.issuer_fp, fp);
    }

    #[test]
    fn rgb_fungible_to_public_uniform_with_webcash(
        amount in arb_amount(),
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let s = SecretFungible {
            amount,
            secret: secret.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let p = s.to_public();
        // Hash is sha256(secret_hex_bytes) — namespace lives in (contract, fp).
        prop_assert_eq!(p.hash, hex::encode(Sha256::digest(secret.as_bytes())));
        prop_assert_eq!(p.contract_id, contract);
        prop_assert_eq!(p.issuer_fp, fp);
    }
}

// ─── rgb collectible (RGB21-shaped, no amount segment) ─────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn rgb_collectible_secret_roundtrip(
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let s = SecretCollectible {
            secret: secret.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let printed = s.to_string();
        let parsed = SecretCollectible::parse(&printed).unwrap();
        prop_assert_eq!(parsed.secret, secret);
        prop_assert_eq!(parsed.contract_id, contract);
        prop_assert_eq!(parsed.issuer_fp, fp);
    }

    #[test]
    fn rgb_collectible_public_roundtrip(
        hash in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let p = PublicCollectible {
            hash: hash.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let printed = p.to_string();
        let parsed = PublicCollectible::parse(&printed).unwrap();
        prop_assert_eq!(parsed.hash, hash);
        prop_assert_eq!(parsed.contract_id, contract);
        prop_assert_eq!(parsed.issuer_fp, fp);
    }

    /// A collectible token must NEVER parse as a fungible token (it has
    /// no amount segment), and a fungible token must NEVER parse as a
    /// collectible (it carries a leading `e{amount}:`).
    #[test]
    fn rgb_collectible_and_fungible_are_disjoint(
        amount in arb_amount(),
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let fungible = SecretFungible {
            amount,
            secret: secret.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        }
        .to_string();
        let collectible = SecretCollectible {
            secret,
            contract_id: contract,
            issuer_fp: fp,
        }
        .to_string();
        prop_assert!(SecretCollectible::parse(&fungible).is_err());
        prop_assert!(SecretFungible::parse(&collectible).is_err());
    }
}

// ─── voucher (RGB-fungible-shaped wire format, distinct semantics) ─────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn voucher_secret_roundtrip(
        amount in arb_amount(),
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let s = SecretVoucher {
            amount,
            secret: secret.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let printed = s.to_string();
        let parsed = SecretVoucher::parse(&printed).unwrap();
        prop_assert_eq!(parsed.amount, amount);
        prop_assert_eq!(parsed.secret, secret);
        prop_assert_eq!(parsed.contract_id, contract);
        prop_assert_eq!(parsed.issuer_fp, fp);
    }

    #[test]
    fn voucher_public_roundtrip(
        amount in arb_amount(),
        hash in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let p = PublicVoucher {
            amount,
            hash: hash.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let printed = p.to_string();
        let parsed = PublicVoucher::parse(&printed).unwrap();
        prop_assert_eq!(parsed.amount, amount);
        prop_assert_eq!(parsed.hash, hash);
        prop_assert_eq!(parsed.contract_id, contract);
        prop_assert_eq!(parsed.issuer_fp, fp);
    }

    #[test]
    fn voucher_to_public_uniform_hash(
        amount in arb_amount(),
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let s = SecretVoucher {
            amount,
            secret: secret.clone(),
            contract_id: contract.clone(),
            issuer_fp: fp.clone(),
        };
        let p = s.to_public();
        prop_assert_eq!(p.hash, hex::encode(Sha256::digest(secret.as_bytes())));
        prop_assert_eq!(p.contract_id, contract);
        prop_assert_eq!(p.issuer_fp, fp);
    }
}

// ─── amount precision ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Every amount round-trips through Display + FromStr without losing wats.
    #[test]
    fn amount_string_roundtrip(wats in 0i64..=i64::MAX / 2) {
        let a = Amount::from_wats(wats);
        let printed = a.to_string();
        let parsed: Amount = printed.parse().unwrap();
        prop_assert_eq!(parsed.wats, wats);
    }
}

// ─── cross-flavor disjointness ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// A webcash token (no namespace) must not parse as RGB fungible or
    /// voucher (which both require contract + fingerprint). Also the
    /// reverse: a namespaced token must not parse as plain webcash.
    #[test]
    fn webcash_is_distinct_from_namespaced(
        amount in arb_amount(),
        secret in arb_secret_hex(),
        contract in arb_contract_slug(),
        fp in arb_fingerprint(),
    ) {
        let webcash = SecretWebcash { amount, secret: secret.clone() }.to_string();
        let voucher = SecretVoucher {
            amount,
            secret,
            contract_id: contract,
            issuer_fp: fp,
        }
        .to_string();
        prop_assert!(SecretFungible::parse(&webcash).is_err());
        prop_assert!(SecretVoucher::parse(&webcash).is_err());
        prop_assert!(SecretWebcash::parse(&voucher).is_err());
    }
}
