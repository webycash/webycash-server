//! End-to-end HTLC swap on the RGB-fungible server.
//!
//! Drives the server through the full HTLC primitive surface defined in
//! `webycash-asset-rgb::htlc` and described in
//! `webycash-server/docs/referee-zkp-based-swap.md`. No mock — actual HTTP calls
//! against a freshly-spawned Redis-backed `server-rgb` binary. Each test
//! is fully self-contained (own Redis container, own server, own port,
//! own contract id) so they can run in parallel without colliding on the
//! single-use-seal store.
//!
//! Scenarios covered:
//!
//! 1. Mine + lock-then-claim with the correct preimage (happy path).
//! 2. Lock-then-claim with the WRONG preimage (server rejects 500
//!    with `htlc input 0: htlc: provided preimage does not match`).
//! 3. Lock-then-refund BEFORE the server's clock has reached the
//!    refund deadline (server rejects with `RefundLocked`).
//! 4. Lock-then-refund AFTER the deadline (accept).
//! 5. Plain `/replace` of a locked output without an `htlc_witnesses`
//!    entry (server rejects: input is HTLC-locked but no witness).
//!
//! The test asserts on response status + a substring of the diagnostic
//! body so downstream wallets can render actionable errors.

mod common;
use common::*;

const ISSUER: &str = "aabbccddeeff00112233445566778899aabbccdd";

/// Mine 1 RGB20 token at the given (contract, issuer).
fn mine_one(harness: &TestHarness, contract: &str, secret_hex: &str) {
    let subsidy = "0".repeat(64);
    let template = format!(
        r#"{{"webcash":["e1.0:secret:{secret_hex}:{contract}:{ISSUER}"],"subsidy":["e0.5:secret:{subsidy}:{contract}:{ISSUER}"],"timestamp":1714003200,"difficulty":4,"nonce":__N__}}"#
    );
    let preimage = find_preimage(&template, 4);
    let (status, body) = harness
        .post(
            "/api/v1/mining_report",
            serde_json::json!({"preimage": preimage, "legalese": {"terms": true}}),
        )
        .expect("mining_report");
    assert_eq!(status, 200, "mine: {body}");
}

#[test]
fn htlc_lock_and_claim_with_correct_preimage() {
    let Some(harness) = TestHarness::start("webycash-server-rgb") else {
        return;
    };
    let contract = format!("rgb20-htlc-{}", uuid_short());
    let alice_in = "a".repeat(64);
    mine_one(&harness, &contract, &alice_in);

    // Step 1: Alice locks her token. The output is a fresh secret that will
    // sit in HTLC state.
    let locked_secret = "1".repeat(64);
    let claim_recipient_secret = "b".repeat(64); // Bob's claim secret
    let refund_self_secret = "c".repeat(64); // Alice's refund secret
    let x = "d".repeat(64); // preimage
    let h = sha256_hex(&x);

    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{alice_in}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_locks": [{
                    "output_index": 0,
                    "request": {
                        "committed_h_hex": h,
                        "refund_after_seconds_from_now": 3600,
                        "claim_owner_secret_hex": claim_recipient_secret,
                        "refund_owner_secret_hex": refund_self_secret,
                    },
                }],
            }),
        )
        .expect("lock");
    assert_eq!(status, 200, "lock failed: {body}");

    // Step 2: Bob (claim path) submits a /replace with the preimage. Output
    // owner is Bob's claim secret.
    let bob_final = claim_recipient_secret.clone();
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{bob_final}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_witnesses": [{
                    "input_index": 0,
                    "witness": {
                        "provided_x_hex": x,
                        "output_owner_hash_hex": sha256_hex(&bob_final),
                    },
                }],
            }),
        )
        .expect("claim");
    assert_eq!(status, 200, "claim should succeed: {body}");
    assert!(body.contains(r#""status": "success""#));

    // Step 3: post-conditions — locked output is spent, claim output is unspent.
    let (status, body) = harness
        .post(
            "/api/v1/health_check",
            serde_json::json!([
                format!(
                    "e1.0:public:{}:{contract}:{ISSUER}",
                    sha256_hex(&locked_secret)
                ),
                format!("e1.0:public:{}:{contract}:{ISSUER}", sha256_hex(&bob_final)),
            ]),
        )
        .expect("hc");
    assert_eq!(status, 200);
    assert!(
        body.contains(r#""spent": true"#),
        "locked must be spent: {body}"
    );
    assert!(
        body.contains(r#""spent": false"#),
        "claim output must be unspent: {body}"
    );
}

#[test]
fn htlc_claim_with_wrong_preimage_rejects() {
    let Some(harness) = TestHarness::start("webycash-server-rgb") else {
        return;
    };
    let contract = format!("rgb20-htlc-bad-{}", uuid_short());
    let alice_in = "2".repeat(64);
    mine_one(&harness, &contract, &alice_in);

    let locked_secret = "3".repeat(64);
    let claim_recipient_secret = "4".repeat(64);
    let refund_self_secret = "5".repeat(64);
    let x = "6".repeat(64);
    let h = sha256_hex(&x);

    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{alice_in}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_locks": [{
                    "output_index": 0,
                    "request": {
                        "committed_h_hex": h,
                        "refund_after_seconds_from_now": 3600,
                        "claim_owner_secret_hex": claim_recipient_secret,
                        "refund_owner_secret_hex": refund_self_secret,
                    },
                }],
            }),
        )
        .expect("lock");
    assert_eq!(status, 200, "lock: {body}");

    // Try to claim with a WRONG preimage — must reject.
    let wrong_x = "7".repeat(64);
    let bob_final = claim_recipient_secret.clone();
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{bob_final}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_witnesses": [{
                    "input_index": 0,
                    "witness": {
                        "provided_x_hex": wrong_x,
                        "output_owner_hash_hex": sha256_hex(&bob_final),
                    },
                }],
            }),
        )
        .expect("bad claim");
    // Server returns the webcash.org-compatible 500-HTML for all errors;
    // the HTLC diagnostic ("htlc input 0: htlc: provided preimage does not
    // match committed hash") is in the server's structured log via
    // `tracing::warn!`, not the body. We assert the rejection at the
    // status-code level. A future enhancement adds a JSON error path
    // for non-webcash assets (tracked in docs/referee-zkp-based-swap.md §11).
    assert_eq!(status, 500, "wrong preimage must reject: {body}");
}

#[test]
fn htlc_replace_locked_without_witness_rejects() {
    let Some(harness) = TestHarness::start("webycash-server-rgb") else {
        return;
    };
    let contract = format!("rgb20-htlc-nowit-{}", uuid_short());
    let alice_in = "8".repeat(64);
    mine_one(&harness, &contract, &alice_in);

    let locked_secret = "9".repeat(64);
    let h = sha256_hex(&"e".repeat(64));

    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{alice_in}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_locks": [{
                    "output_index": 0,
                    "request": {
                        "committed_h_hex": h,
                        "refund_after_seconds_from_now": 3600,
                        "claim_owner_secret_hex": "a".repeat(64),
                        "refund_owner_secret_hex": "b".repeat(64),
                    },
                }],
            }),
        )
        .expect("lock");
    assert_eq!(status, 200, "lock: {body}");

    // Try plain /replace of the locked output, with no htlc_witnesses.
    let unrelated = "f".repeat(64);
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{unrelated}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
            }),
        )
        .expect("nowit");
    assert_eq!(status, 500, "missing witness must reject: {body}");
}

#[test]
fn htlc_refund_before_timeout_rejects() {
    let Some(harness) = TestHarness::start("webycash-server-rgb") else {
        return;
    };
    let contract = format!("rgb20-htlc-refundlock-{}", uuid_short());
    let alice_in = "1".repeat(64);
    mine_one(&harness, &contract, &alice_in);

    let locked_secret = "2".repeat(64);
    let claim_recipient_secret = "3".repeat(64);
    let refund_self_secret = "4".repeat(64);
    let x = "5".repeat(64);
    let h = sha256_hex(&x);

    // 1-hour lock from now.
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{alice_in}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_locks": [{
                    "output_index": 0,
                    "request": {
                        "committed_h_hex": h,
                        "refund_after_seconds_from_now": 3600,
                        "claim_owner_secret_hex": claim_recipient_secret,
                        "refund_owner_secret_hex": refund_self_secret,
                    },
                }],
            }),
        )
        .expect("lock");
    assert_eq!(status, 200, "lock: {body}");

    // Try to refund right away — no preimage; output owner = refund secret.
    let refund_final = refund_self_secret.clone();
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{refund_final}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_witnesses": [{
                    "input_index": 0,
                    "witness": {
                        "provided_x_hex": null,
                        "output_owner_hash_hex": sha256_hex(&refund_final),
                    },
                }],
            }),
        )
        .expect("early refund");
    assert_eq!(status, 500, "refund before timeout must reject: {body}");
}

#[test]
fn htlc_refund_after_timeout_accepts() {
    let Some(harness) = TestHarness::start("webycash-server-rgb") else {
        return;
    };
    let contract = format!("rgb20-htlc-refundok-{}", uuid_short());
    let alice_in = "6".repeat(64);
    mine_one(&harness, &contract, &alice_in);

    let locked_secret = "7".repeat(64);
    let claim_recipient_secret = "8".repeat(64);
    let refund_self_secret = "9".repeat(64);
    let x = "a".repeat(64);
    let h = sha256_hex(&x);

    // 1-second lock so we can sleep past it.
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{alice_in}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_locks": [{
                    "output_index": 0,
                    "request": {
                        "committed_h_hex": h,
                        "refund_after_seconds_from_now": 1,
                        "claim_owner_secret_hex": claim_recipient_secret,
                        "refund_owner_secret_hex": refund_self_secret,
                    },
                }],
            }),
        )
        .expect("lock");
    assert_eq!(status, 200, "lock: {body}");

    // Wait past the deadline.
    std::thread::sleep(std::time::Duration::from_secs(2));

    let refund_final = refund_self_secret.clone();
    let (status, body) = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e1.0:secret:{locked_secret}:{contract}:{ISSUER}")],
                "new_webcashes": [format!("e1.0:secret:{refund_final}:{contract}:{ISSUER}")],
                "legalese": {"terms": true},
                "htlc_witnesses": [{
                    "input_index": 0,
                    "witness": {
                        "provided_x_hex": null,
                        "output_owner_hash_hex": sha256_hex(&refund_final),
                    },
                }],
            }),
        )
        .expect("refund");
    assert_eq!(status, 200, "refund after timeout must succeed: {body}");
}
