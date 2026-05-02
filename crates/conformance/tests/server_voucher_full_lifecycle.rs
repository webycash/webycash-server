//! End-to-end lifecycle for the Voucher flavor with issuer namespacing.

mod common;

use common::*;

#[test]
fn voucher_lifecycle_with_namespace_enforcement() {
    let bin_name = "webycash-server-voucher";
    let Some(harness) = TestHarness::start(bin_name) else {
        return;
    };
    let issuer = "aabbccddeeff00112233445566778899aabbccdd";
    let contract = "credits-q1";

    let secret = "c".repeat(64);
    let public_hash = sha256_hex(&secret);

    // 1. Mine 10 credits.
    let preimage_obj = serde_json::json!({
        "webcash": [format!("e10.0:secret:{secret}:{contract}:{issuer}")],
        "subsidy": [],
        "timestamp": 1714003200i64,
        "difficulty": 4,
        "nonce": 0,
    });
    let preimage_str = serde_json::to_string(&preimage_obj).unwrap();
    // serde_json::to_string emits no spaces, but our nonce-replace assumes the exact "nonce":0 form.
    let template = preimage_str.replace(r#""nonce":0"#, r#""nonce":__N__"#);
    let preimage = find_preimage(&template, 4);

    let mining_resp = harness
        .post(
            "/api/v1/mining_report",
            serde_json::json!({"preimage": preimage, "legalese": {"terms": true}}),
        )
        .unwrap();
    assert_eq!(mining_resp.0, 200, "mine: {}", mining_resp.1);

    // 2. health_check after mine: spent: false
    let hc_body = serde_json::json!([format!(
        "e10.0:public:{public_hash}:{contract}:{issuer}"
    )]);
    let hc_resp = harness.post("/api/v1/health_check", hc_body).unwrap();
    assert_eq!(hc_resp.0, 200);
    assert!(hc_resp.1.contains(r#""spent": false"#), "hc1: {}", hc_resp.1);

    // 3. Split 10 → 3 + 7 within same namespace.
    let out1 = "d".repeat(64);
    let out2 = "e".repeat(64);
    let out1_hash = sha256_hex(&out1);
    let out2_hash = sha256_hex(&out2);
    let replace_resp = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e10.0:secret:{secret}:{contract}:{issuer}")],
                "new_webcashes": [
                    format!("e3.0:secret:{out1}:{contract}:{issuer}"),
                    format!("e7.0:secret:{out2}:{contract}:{issuer}"),
                ],
                "legalese": {"terms": true},
            }),
        )
        .unwrap();
    assert_eq!(replace_resp.0, 200, "replace: {}", replace_resp.1);

    // 4. Verify spent state after split.
    let hc2 = harness
        .post(
            "/api/v1/health_check",
            serde_json::json!([
                format!("e10.0:public:{public_hash}:{contract}:{issuer}"),
                format!("e3.0:public:{out1_hash}:{contract}:{issuer}"),
                format!("e7.0:public:{out2_hash}:{contract}:{issuer}"),
            ]),
        )
        .unwrap();
    assert_eq!(hc2.0, 200);
    assert!(
        hc2.1.contains(&format!(
            r#""e10:public:{public_hash}:{contract}:{issuer}": {{"spent": true, "amount": null}}"#
        )),
        "hc2 input: {}",
        hc2.1
    );
    assert!(
        hc2.1.contains(&format!(
            r#""e3:public:{out1_hash}:{contract}:{issuer}": {{"spent": false, "amount": "3"}}"#
        )),
        "hc2 out1: {}",
        hc2.1
    );

    // 5. Cross-namespace replace MUST be refused.
    let alt_issuer = "f".repeat(40);
    let xn = harness
        .post(
            "/api/v1/replace",
            serde_json::json!({
                "webcashes": [format!("e3.0:secret:{out1}:{contract}:{issuer}")],
                "new_webcashes": [format!(
                    "e3.0:secret:{out_secret}:{contract}:{alt_issuer}",
                    out_secret = "1".repeat(64)
                )],
                "legalese": {"terms": true},
            }),
        )
        .unwrap();
    assert_eq!(xn.0, 500, "cross-ns must 500");

    // 6. Burn 3.0.
    let burn = harness
        .post(
            "/api/v1/burn",
            serde_json::json!({
                "webcash": format!("e3.0:secret:{out1}:{contract}:{issuer}"),
                "legalese": {"terms": true},
            }),
        )
        .unwrap();
    assert_eq!(burn.0, 200, "burn: {}", burn.1);

    // 7. After burn: 3 spent, 7 still unspent.
    let hc3 = harness
        .post(
            "/api/v1/health_check",
            serde_json::json!([
                format!("e3.0:public:{out1_hash}:{contract}:{issuer}"),
                format!("e7.0:public:{out2_hash}:{contract}:{issuer}"),
            ]),
        )
        .unwrap();
    assert_eq!(hc3.0, 200);
    assert!(
        hc3.1.contains(&format!(
            r#""e3:public:{out1_hash}:{contract}:{issuer}": {{"spent": true, "amount": null}}"#
        )),
        "hc3 burn: {}",
        hc3.1
    );
    assert!(
        hc3.1.contains(&format!(
            r#""e7:public:{out2_hash}:{contract}:{issuer}": {{"spent": false, "amount": "7"}}"#
        )),
        "hc3 unspent: {}",
        hc3.1
    );
}
