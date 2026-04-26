//! Property tests for the storage key partitioning invariants.
//!
//! The server's safety story rests on storage keys never colliding
//! across asset flavors or namespaces. A bad `KeyStrategy` would let
//! one issuer's RGB token alias another issuer's Voucher token at the
//! Redis / DynamoDB / FoundationDB level — breaking issuer isolation
//! and cross-namespace replace rejection.
//!
//! These tests pin the invariants:
//!   1. Determinism: same inputs → same key, always.
//!   2. WebcashLegacyKeys ignores asset name and namespace (frozen wire).
//!   3. NamespacedKeys distinguishes any two distinct (asset, ns, hash) tuples.
//!   4. Token / replacement / burn / mining keys never collide with each
//!      other (each role lives in its own keyspace prefix).

use proptest::prelude::*;

use webycash_asset_core::{ContractId, PgpFingerprint};
use webycash_storage::{KeyStrategy, Namespace, NamespacedKeys, WebcashLegacyKeys};

// ─── strategies ────────────────────────────────────────────────────────

fn arb_contract() -> impl Strategy<Value = ContractId> {
    "[a-zA-Z0-9_-]{1,64}".prop_map(ContractId)
}
fn arb_fingerprint() -> impl Strategy<Value = PgpFingerprint> {
    "[0-9a-f]{40}".prop_map(PgpFingerprint)
}
fn arb_hash() -> impl Strategy<Value = String> {
    "[0-9a-f]{64}"
}
fn arb_op_id() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,32}"
}
fn arb_asset_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["webcash", "rgb", "voucher", "rgb-collectible"])
        .prop_map(|s| s.to_string())
}
fn arb_namespace() -> impl Strategy<Value = Namespace> {
    prop_oneof![
        Just(Namespace::unscoped()),
        (arb_contract(), arb_fingerprint())
            .prop_map(|(c, f)| Namespace::scoped(c, f)),
    ]
}

// ─── determinism ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn webcash_legacy_token_key_is_deterministic(
        asset in arb_asset_name(),
        ns in arb_namespace(),
        hash in arb_hash(),
    ) {
        let s = WebcashLegacyKeys;
        prop_assert_eq!(s.token_key(&asset, &ns, &hash), s.token_key(&asset, &ns, &hash));
    }

    /// WebcashLegacyKeys must IGNORE asset name and namespace — the
    /// only input that affects the key is the public hash.
    /// (This is the frozen-wire-format invariant the production
    /// webcash.org server relies on.)
    #[test]
    fn webcash_legacy_token_key_ignores_asset_and_namespace(
        asset_a in arb_asset_name(),
        asset_b in arb_asset_name(),
        ns_a in arb_namespace(),
        ns_b in arb_namespace(),
        hash in arb_hash(),
    ) {
        let s = WebcashLegacyKeys;
        prop_assert_eq!(
            s.token_key(&asset_a, &ns_a, &hash),
            s.token_key(&asset_b, &ns_b, &hash),
        );
    }

    #[test]
    fn namespaced_token_key_is_deterministic(
        asset in arb_asset_name(),
        ns in arb_namespace(),
        hash in arb_hash(),
    ) {
        let s = NamespacedKeys;
        prop_assert_eq!(s.token_key(&asset, &ns, &hash), s.token_key(&asset, &ns, &hash));
    }
}

// ─── partitioning ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Distinct asset names must produce distinct keys for the same
    /// (namespace, hash). Cross-asset collision would let a Voucher
    /// token alias an RGB token at the storage layer.
    #[test]
    fn namespaced_distinct_assets_yield_distinct_keys(
        asset_a in arb_asset_name(),
        asset_b in arb_asset_name(),
        ns in arb_namespace(),
        hash in arb_hash(),
    ) {
        prop_assume!(asset_a != asset_b);
        let s = NamespacedKeys;
        prop_assert_ne!(
            s.token_key(&asset_a, &ns, &hash),
            s.token_key(&asset_b, &ns, &hash),
        );
    }

    /// Distinct (contract, issuer) namespaces must produce distinct
    /// keys for the same (asset, hash). Cross-namespace collision
    /// would break issuer isolation.
    #[test]
    fn namespaced_distinct_namespaces_yield_distinct_keys(
        asset in arb_asset_name(),
        c_a in arb_contract(),
        c_b in arb_contract(),
        f_a in arb_fingerprint(),
        f_b in arb_fingerprint(),
        hash in arb_hash(),
    ) {
        prop_assume!(c_a != c_b || f_a != f_b);
        let s = NamespacedKeys;
        let ns_a = Namespace::scoped(c_a, f_a);
        let ns_b = Namespace::scoped(c_b, f_b);
        prop_assert_ne!(
            s.token_key(&asset, &ns_a, &hash),
            s.token_key(&asset, &ns_b, &hash),
        );
    }

    /// Distinct hashes must produce distinct keys for the same
    /// (asset, namespace). Trivial but worth pinning.
    #[test]
    fn namespaced_distinct_hashes_yield_distinct_keys(
        asset in arb_asset_name(),
        ns in arb_namespace(),
        h_a in arb_hash(),
        h_b in arb_hash(),
    ) {
        prop_assume!(h_a != h_b);
        let s = NamespacedKeys;
        prop_assert_ne!(
            s.token_key(&asset, &ns, &h_a),
            s.token_key(&asset, &ns, &h_b),
        );
    }
}

// ─── role separation ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// token / replace-audit / burn-audit / mining-state keys must never
    /// collide with each other. This is what keeps `SCAN token:*` from
    /// returning replacement audit records (or vice versa).
    #[test]
    fn namespaced_role_keys_never_collide(
        asset in arb_asset_name(),
        ns in arb_namespace(),
        hash in arb_hash(),
        op in arb_op_id(),
    ) {
        let s = NamespacedKeys;
        let token = s.token_key(&asset, &ns, &hash);
        let replace = s.replacement_key(&asset, &ns, &op);
        let burn = s.burn_key(&asset, &ns, &op);
        let mining = s.mining_state_key(&asset);
        let all = [&token, &replace, &burn, &mining];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                prop_assert_ne!(*a, *b);
            }
        }
    }

    #[test]
    fn webcash_legacy_role_keys_never_collide(
        asset in arb_asset_name(),
        ns in arb_namespace(),
        hash in arb_hash(),
        op in arb_op_id(),
    ) {
        let s = WebcashLegacyKeys;
        let token = s.token_key(&asset, &ns, &hash);
        let replace = s.replacement_key(&asset, &ns, &op);
        let burn = s.burn_key(&asset, &ns, &op);
        let mining = s.mining_state_key(&asset);
        let all = [&token, &replace, &burn, &mining];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                prop_assert_ne!(*a, *b);
            }
        }
    }
}
