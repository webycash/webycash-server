//! Invariants over the captured `webcash.org` production fixtures.
//!
//! The Webcash flavor is byte-frozen against production: any drift from
//! these invariants is a wire-format break. Each test pins a property
//! that the live `https://webcash.org` server exhibits, so when we add
//! new fixtures the server-webcash binary must keep matching them.

use webycash_conformance::fixtures::{self, Fixture};

/// Tornado returns JSON bodies with `Content-Type: text/html;
/// charset=UTF-8` rather than `application/json`. Pin it.
#[test]
fn json_endpoints_carry_text_html_content_type() {
    let endpoints_returning_json = ["get_target", "post_health_check_unknown"];
    for stem in endpoints_returning_json {
        let fx = fixtures::load(stem).unwrap_or_else(|e| panic!("load {stem}: {e}"));
        let ct = fx
            .response
            .headers
            .get("Content-Type")
            .map(String::as_str)
            .unwrap_or("");
        assert!(
            ct.contains("text/html"),
            "{stem}: expected text/html Content-Type (Tornado quirk), got {ct:?}",
        );
    }
}

/// Every captured fixture has a `captured_at` timestamp and a non-empty
/// request method/url. Catches accidentally truncated checkins.
#[test]
fn every_fixture_has_required_metadata() {
    let all = fixtures::load_all().expect("load_all");
    for (stem, fx) in all {
        assert_metadata(&stem, &fx);
    }
}

fn assert_metadata(stem: &str, fx: &Fixture) {
    assert!(!fx.captured_at.is_empty(), "{stem}: empty captured_at");
    assert!(!fx.request.method.is_empty(), "{stem}: empty method");
    assert!(!fx.request.url.is_empty(), "{stem}: empty url");
    assert!(fx.request.url.starts_with("http"), "{stem}: bad url scheme");
    assert!(
        fx.response.status >= 100,
        "{stem}: bogus status {}",
        fx.response.status
    );
}

/// Health-check happy path returns a `status: success` envelope. The
/// `unknown` fixture is the canonical "novel hash → null" shape.
#[test]
fn health_check_unknown_returns_success_envelope() {
    let fx = fixtures::load("post_health_check_unknown").expect("fixture");
    let body = fx.response.body_parsed.as_ref().expect("parsed body");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("success"),
        "expected status=success: {body}"
    );
    let results = body.get("results").expect("results map");
    assert!(results.is_object(), "results must be an object");
}

/// `get_target` returns a body with the four documented numeric fields.
#[test]
fn get_target_carries_documented_fields() {
    let fx = fixtures::load("get_target").expect("fixture");
    assert_eq!(fx.response.status, 200);
    let body = fx.response.body_parsed.as_ref().expect("parsed body");
    for field in [
        "difficulty_target_bits",
        "ratio",
        "mining_amount",
        "mining_subsidy_amount",
    ] {
        assert!(
            body.get(field).is_some(),
            "get_target body missing {field}: {body}",
        );
    }
}

/// Production responds 404 with a Tornado-style HTML body for
/// unrecognised paths (here: the never-implemented `/api/v1/stats`
/// endpoint we deliberately don't expose).
#[test]
fn stats_404_is_html_not_json() {
    let fx = fixtures::load("get_stats_404").expect("fixture");
    assert_eq!(fx.response.status, 404);
    let ct = fx
        .response
        .headers
        .get("Content-Type")
        .map(String::as_str)
        .unwrap_or("");
    assert!(ct.contains("text/html"), "404 wasn't text/html: got {ct:?}");
}

/// Every POST fixture's body carries `legalese.terms = true` — the
/// production server rejects state-mutating endpoints without it.
#[test]
fn post_fixtures_carry_legalese_terms_true() {
    let all = fixtures::load_all().expect("load_all");
    for (stem, fx) in all {
        if fx.request.method != "POST" {
            continue;
        }
        let Some(body) = &fx.request.body else { continue };
        let Some(terms) = body.pointer("/legalese/terms") else {
            // A few captured fixtures (like deliberately-malformed
            // examples) intentionally omit legalese — those don't
            // claim to demonstrate a happy path.
            assert!(
                stem.contains("malformed") || stem.contains("empty"),
                "{stem}: POST body missing legalese.terms"
            );
            continue;
        };
        assert_eq!(
            terms.as_bool(),
            Some(true),
            "{stem}: legalese.terms != true"
        );
    }
}
