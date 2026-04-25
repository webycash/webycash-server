//! End-to-end lifecycle for the RGB-Fungible flavor with issuer namespacing.

mod common;

use common::*;

#[test]
fn rgb_fungible_lifecycle_with_namespace_enforcement() {
    let bin_name = "webycash-server-rgb";
    let Some(harness) = TestHarness::start(bin_name) else {
        return;
    };
    let issuer = "aabbccddeeff00112233445566778899aabbccdd";
    let contract = "rgb20-usdc";

    let secret = "a".repeat(64);
    let public_hash = sha256_hex(&secret);

    let preimage_obj = serde_json::json!({
        "webcash": [format!("e100.0:secret:{secret}:{contract}:{issuer}")],
        "subsidy": [],
        "timestamp": 1714003200i64,
        "difficulty": 4,
        "nonce": 0,
    });
    let template = serde_json::to_string(&preimage_obj)
        .unwrap()
        .replace(r#""nonce":0"#, r#""nonce":__N__"#);
    let preimage = find_preimage(&template, 4);

    let mine = harness
        .post(
            "/api/v1/mining_report",
            serde_json::json!({"preimage": preimage, "legalese": {"terms": true}}),
        )
        .unwrap();
    assert_eq!(mine.0, 200, "mine: {}", mine.1);

    let hc1 = harness
        .post(
            "/api/v1/health_check",
            serde_json::json!([format!(
                "e100.0:public:{public_hash}:{contract}:{issuer}"
            )]),
        )
        .unwrap();
    assert_eq!(hc1.0, 200);
    assert!(hc1.1.contains(r#""spent": false"#), "hc1: {}", hc1.1);

    // Split 100 → 25 + 75
    let out1 = "b".repeat(64);
    let out2 = "c".repeat(64);
    let out1_hash = sha256_hex(&out1);
    let out2_hash = sha256_hex(&out2);
    let split = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e100.0:secret:{secret}:{contract}:{issuer}")],
                "new_webcashes": [
                    format!("e25.0:secret:{out1}:{contract}:{issuer}"),
                    format!("e75.0:secret:{out2}:{contract}:{issuer}"),
                ],
                "legalese": {"terms": true},
            }),
        )
        .unwrap();
    assert_eq!(split.0, 200, "split: {}", split.1);

    let hc2 = harness
        .post(
            "/api/v1/health_check",
            serde_json::json!([
                format!("e100.0:public:{public_hash}:{contract}:{issuer}"),
                format!("e25.0:public:{out1_hash}:{contract}:{issuer}"),
                format!("e75.0:public:{out2_hash}:{contract}:{issuer}"),
            ]),
        )
        .unwrap();
    assert_eq!(hc2.0, 200);
    assert!(
        hc2.1.contains(&format!(
            r#""e100:public:{public_hash}:{contract}:{issuer}": {{"spent": true}}"#
        )),
        "hc2 input: {}",
        hc2.1
    );
    assert!(
        hc2.1.contains(&format!(
            r#""e25:public:{out1_hash}:{contract}:{issuer}": {{"spent": false}}"#
        )),
        "hc2 out1: {}",
        hc2.1
    );

    // Cross-contract replace must fail (different contract_id).
    let alt_contract = "rgb20-usdt";
    let xn = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e25.0:secret:{out1}:{contract}:{issuer}")],
                "new_webcashes": [format!(
                    "e25.0:secret:{secret}:{alt_contract}:{issuer}",
                    secret = "9".repeat(64)
                )],
                "legalese": {"terms": true},
            }),
        )
        .unwrap();
    assert_eq!(xn.0, 500, "cross-contract must 500");

    // Burn 25.
    let burn = harness
        .post(
            "/api/v1/burn",
            serde_json::json!({
                "webcash": format!("e25.0:secret:{out1}:{contract}:{issuer}"),
                "legalese": {"terms": true},
            }),
        )
        .unwrap();
    assert_eq!(burn.0, 200, "burn: {}", burn.1);
}
