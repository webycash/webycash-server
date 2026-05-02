//! HashRecord codec invariants: every per-flavor record must
//! roundtrip cleanly through `to_fields` → `from_fields`. Catches the
//! bug class where a field name drifts between writer and reader, or
//! where a value escaping rule breaks under arbitrary input.
//!
//! The records carry timestamps (chrono::DateTime<Utc>) — we use the
//! library's RFC 3339 encoding, which preserves wall time to the
//! nanosecond. Generated timestamps are clamped to a representable
//! window so chrono's parser doesn't reject them.

use std::collections::HashMap;

use proptest::prelude::*;

use webycash_asset_core::{ContractId, PgpFingerprint};
use webycash_asset_rgb::{RgbCollectibleRecord, RgbFungibleRecord, RgbOrigin};
use webycash_asset_voucher::{VoucherOrigin, VoucherRecord};
use webycash_asset_webcash::{WebcashOrigin, WebcashRecord};
use webycash_storage::HashRecord;

// ─── strategies ────────────────────────────────────────────────────────

fn arb_hash() -> impl Strategy<Value = String> {
    "[0-9a-f]{64}"
}
fn arb_contract() -> impl Strategy<Value = ContractId> {
    "[a-zA-Z0-9_-]{1,64}".prop_map(ContractId)
}
fn arb_fingerprint() -> impl Strategy<Value = PgpFingerprint> {
    "[0-9a-f]{40}".prop_map(PgpFingerprint)
}

/// Timestamps in a safe window. RFC 3339 only represents a finite
/// range; we cap to year 9999 to keep proptest from generating values
/// the chrono parser rejects.
fn arb_datetime() -> impl Strategy<Value = chrono::DateTime<chrono::Utc>> {
    (0i64..=253_402_300_799_999i64).prop_map(|millis| {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)
            .unwrap_or_else(chrono::Utc::now)
    })
}

fn arb_webcash_origin() -> impl Strategy<Value = WebcashOrigin> {
    prop_oneof![Just(WebcashOrigin::Mined), Just(WebcashOrigin::Replaced)]
}

fn arb_rgb_origin() -> impl Strategy<Value = RgbOrigin> {
    prop_oneof![
        Just(RgbOrigin::Mined),
        Just(RgbOrigin::Issued),
        Just(RgbOrigin::Replaced),
    ]
}

fn arb_voucher_origin() -> impl Strategy<Value = VoucherOrigin> {
    prop_oneof![
        Just(VoucherOrigin::Mined),
        Just(VoucherOrigin::Issued),
        Just(VoucherOrigin::Replaced),
    ]
}

fn arb_webcash_record() -> impl Strategy<Value = WebcashRecord> {
    (
        arb_hash(),
        1i64..=i64::MAX / 4,
        any::<bool>(),
        arb_datetime(),
        proptest::option::of(arb_datetime()),
        arb_webcash_origin(),
    )
        .prop_map(
            |(public_hash, amount_wats, spent, created_at, spent_at, origin)| WebcashRecord {
                public_hash,
                amount_wats,
                spent,
                created_at,
                spent_at,
                origin,
            },
        )
}

fn arb_rgb_fungible_record() -> impl Strategy<Value = RgbFungibleRecord> {
    (
        arb_hash(),
        1i64..=i64::MAX / 4,
        any::<bool>(),
        arb_datetime(),
        proptest::option::of(arb_datetime()),
        arb_rgb_origin(),
        arb_contract(),
        arb_fingerprint(),
    )
        .prop_map(
            |(
                public_hash,
                amount_wats,
                spent,
                created_at,
                spent_at,
                origin,
                contract_id,
                issuer_fp,
            )| RgbFungibleRecord {
                public_hash,
                amount_wats,
                spent,
                created_at,
                spent_at,
                origin,
                contract_id,
                issuer_fp,
                // Codec round-trip property covers plain (unlocked) records;
                // HTLC-locked round-trips have dedicated coverage in
                // server_rgb_htlc_swap.rs and server_rgb21_htlc.rs.
                htlc_state: None,
            },
        )
}

fn arb_rgb_collectible_record() -> impl Strategy<Value = RgbCollectibleRecord> {
    (
        arb_hash(),
        any::<bool>(),
        arb_datetime(),
        proptest::option::of(arb_datetime()),
        arb_rgb_origin(),
        arb_contract(),
        arb_fingerprint(),
    )
        .prop_map(
            |(public_hash, spent, created_at, spent_at, origin, contract_id, issuer_fp)| {
                RgbCollectibleRecord {
                    public_hash,
                    spent,
                    created_at,
                    spent_at,
                    origin,
                    contract_id,
                    issuer_fp,
                    htlc_state: None,
                }
            },
        )
}

fn arb_voucher_record() -> impl Strategy<Value = VoucherRecord> {
    (
        arb_hash(),
        1i64..=i64::MAX / 4,
        any::<bool>(),
        arb_datetime(),
        proptest::option::of(arb_datetime()),
        arb_voucher_origin(),
        arb_contract(),
        arb_fingerprint(),
    )
        .prop_map(
            |(
                public_hash,
                amount_wats,
                spent,
                created_at,
                spent_at,
                origin,
                contract_id,
                issuer_fp,
            )| VoucherRecord {
                public_hash,
                amount_wats,
                spent,
                created_at,
                spent_at,
                origin,
                contract_id,
                issuer_fp,
            },
        )
}

fn roundtrip<R: HashRecord>(rec: &R) -> Option<R> {
    let mut fields = HashMap::new();
    rec.to_fields(&mut fields);
    R::from_fields(rec.public_hash(), &fields)
}

// ─── webcash ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn webcash_record_roundtrip_preserves_all_fields(rec in arb_webcash_record()) {
        let back = roundtrip(&rec).expect("from_fields");
        prop_assert_eq!(back.public_hash, rec.public_hash);
        prop_assert_eq!(back.amount_wats, rec.amount_wats);
        prop_assert_eq!(back.spent, rec.spent);
        prop_assert_eq!(back.created_at, rec.created_at);
        prop_assert_eq!(back.spent_at, rec.spent_at);
        prop_assert_eq!(back.origin, rec.origin);
    }

    #[test]
    fn rgb_fungible_record_roundtrip(rec in arb_rgb_fungible_record()) {
        let back = roundtrip(&rec).expect("from_fields");
        prop_assert_eq!(back.public_hash, rec.public_hash);
        prop_assert_eq!(back.amount_wats, rec.amount_wats);
        prop_assert_eq!(back.spent, rec.spent);
        prop_assert_eq!(back.created_at, rec.created_at);
        prop_assert_eq!(back.spent_at, rec.spent_at);
        prop_assert_eq!(back.origin, rec.origin);
        prop_assert_eq!(back.contract_id, rec.contract_id);
        prop_assert_eq!(back.issuer_fp, rec.issuer_fp);
    }

    #[test]
    fn rgb_collectible_record_roundtrip(rec in arb_rgb_collectible_record()) {
        let back = roundtrip(&rec).expect("from_fields");
        // Collectible amount_wats is always 0 (uniform with fungible
        // HASH layout so a single Lua script handles both flavors).
        prop_assert_eq!(back.amount_wats(), 0);
        prop_assert_eq!(back.public_hash, rec.public_hash);
        prop_assert_eq!(back.spent, rec.spent);
        prop_assert_eq!(back.created_at, rec.created_at);
        prop_assert_eq!(back.spent_at, rec.spent_at);
        prop_assert_eq!(back.origin, rec.origin);
        prop_assert_eq!(back.contract_id, rec.contract_id);
        prop_assert_eq!(back.issuer_fp, rec.issuer_fp);
    }

    #[test]
    fn voucher_record_roundtrip(rec in arb_voucher_record()) {
        let back = roundtrip(&rec).expect("from_fields");
        prop_assert_eq!(back.public_hash, rec.public_hash);
        prop_assert_eq!(back.amount_wats, rec.amount_wats);
        prop_assert_eq!(back.spent, rec.spent);
        prop_assert_eq!(back.created_at, rec.created_at);
        prop_assert_eq!(back.spent_at, rec.spent_at);
        prop_assert_eq!(back.origin, rec.origin);
        prop_assert_eq!(back.contract_id, rec.contract_id);
        prop_assert_eq!(back.issuer_fp, rec.issuer_fp);
    }

    /// Namespace lives in the wire token + DB key, NOT in the record's
    /// HASH fields. So a record's `namespace()` must agree with what
    /// it carries in `(contract_id, issuer_fp)`.
    #[test]
    fn rgb_fungible_namespace_matches_record(rec in arb_rgb_fungible_record()) {
        let ns = rec.namespace();
        prop_assert_eq!(ns.contract_id.as_ref(), Some(&rec.contract_id));
        prop_assert_eq!(ns.issuer_fp.as_ref(), Some(&rec.issuer_fp));
    }

    #[test]
    fn voucher_namespace_matches_record(rec in arb_voucher_record()) {
        let ns = rec.namespace();
        prop_assert_eq!(ns.contract_id.as_ref(), Some(&rec.contract_id));
        prop_assert_eq!(ns.issuer_fp.as_ref(), Some(&rec.issuer_fp));
    }

    /// Webcash records are unscoped (the legacy testnet schema has no
    /// namespace at all). This is the wire-protocol-frozen invariant.
    #[test]
    fn webcash_namespace_is_unscoped(rec in arb_webcash_record()) {
        let ns = rec.namespace();
        prop_assert!(ns.contract_id.is_none(), "webcash must be unscoped");
        prop_assert!(ns.issuer_fp.is_none(), "webcash must be unscoped");
    }
}
