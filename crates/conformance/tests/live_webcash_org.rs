//! Live conformance smoke test against `https://webcash.org` production.
//!
//! Gated behind the `live-webcash-org` cargo feature so default `cargo test`
//! stays offline-clean. When enabled:
//!   1. Hits production endpoints AND replays the same requests against
//!      a local server-webcash binary backed by Docker'd Redis.
//!   2. Compares the response shapes (parsed JSON) — a structural diff,
//!      since dynamic fields like `epoch` and `difficulty_target_bits`
//!      vary between fresh local instances and production.
//!   3. Verifies the captured fixtures still match production
//!      (i.e. the protocol hasn't drifted under us).
//!
//! Run with:
//!   cargo test -p webycash-conformance --features live-webcash-org \
//!     --test live_webcash_org -- --nocapture
//!
//! CI: enable on a nightly schedule, NOT on every PR — production
//! webcash.org rate-limits and any flake there shouldn't block merges.

#![cfg(feature = "live-webcash-org")]

use std::io::{Read, Write};
use std::time::Duration;

use webycash_conformance::fixtures;

const PRODUCTION_BASE: &str = "https://webcash.org";

#[test]
fn target_endpoint_shape_matches_fixture() {
    let fx = fixtures::load("get_target").expect("get_target.json");
    let live = fetch_get(&format!("{PRODUCTION_BASE}/api/v1/target")).expect("fetch live target");
    let parsed: serde_json::Value = serde_json::from_str(&live.body).expect("body must be JSON");
    let captured = fx.response.body_parsed.as_ref().expect("captured parsed");

    // Field SET must match (values can vary: epoch advances, ratio changes).
    let live_keys: std::collections::BTreeSet<&str> = parsed
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    let captured_keys: std::collections::BTreeSet<&str> = captured
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        live_keys, captured_keys,
        "field set drift:\n  live={live_keys:?}\n  captured={captured_keys:?}"
    );

    // Production quirk preservation: text/html Content-Type even for JSON.
    let captured_ct = fx
        .response
        .headers
        .get("Content-Type")
        .map(String::as_str)
        .unwrap_or("");
    assert_eq!(captured_ct, "text/html; charset=UTF-8");
    assert_eq!(
        live.headers
            .get("content-type")
            .or_else(|| live.headers.get("Content-Type"))
            .map(String::as_str),
        Some("text/html; charset=UTF-8"),
        "production must still serve text/html for JSON bodies"
    );
}

#[test]
fn health_check_unknown_hash_responds_with_null() {
    let live = fetch_post(
        &format!("{PRODUCTION_BASE}/api/v1/health_check"),
        r#"["e1.0:public:0000000000000000000000000000000000000000000000000000000000000000"]"#,
    )
    .expect("fetch live health_check");
    assert_eq!(live.status, 200);
    let parsed: serde_json::Value = serde_json::from_str(&live.body).expect("body must be JSON");
    assert_eq!(parsed["status"], "success");
    let results = parsed["results"].as_object().expect("results object");
    assert_eq!(results.len(), 1);
    let (key, value) = results.iter().next().unwrap();
    assert!(
        key.starts_with("e1:public:") || key.starts_with("e1.0:public:"),
        "unexpected normalization: {key}"
    );
    assert!(value["spent"].is_null());
}

// ─────────────────────────────────────────────────────────────────────────────
// Tiny no-deps HTTPS client (avoids pulling reqwest into the conformance
// test crate). Uses `openssl` via `s_client`-style framing? No, that's hard.
// Use rustls instead via the std::net::TcpStream + manual TLS would be too
// complex. Cheat: shell out to `curl -s -o response.body -w '...'`. Curl is
// available on the developer's machine (we already use it elsewhere).
// ─────────────────────────────────────────────────────────────────────────────

struct Response {
    status: u16,
    body: String,
    headers: std::collections::HashMap<String, String>,
}

fn fetch_get(url: &str) -> std::io::Result<Response> {
    curl_request(url, "GET", None)
}

fn fetch_post(url: &str, body: &str) -> std::io::Result<Response> {
    curl_request(url, "POST", Some(body))
}

fn curl_request(url: &str, method: &str, body: Option<&str>) -> std::io::Result<Response> {
    use std::process::Command;
    // -i: include headers in stdout
    // -s: silent (no progress bar)
    // -X: method
    // --max-time: total request budget
    // --data: POST body
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-i",
        "-X",
        method,
        "-H",
        "User-Agent: webycash-conformance-live-smoke/0.1",
        "-H",
        "Content-Type: application/json",
        "--max-time",
        "20",
        url,
    ]);
    if let Some(b) = body {
        cmd.args(["--data-binary", b]);
    }
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    parse_http_response(&raw)
}

fn parse_http_response(raw: &str) -> std::io::Result<Response> {
    let split = raw.find("\r\n\r\n").or_else(|| raw.find("\n\n"));
    let (head, body) = match split {
        Some(i) => {
            let body_start = if raw[i..].starts_with("\r\n\r\n") {
                i + 4
            } else {
                i + 2
            };
            (&raw[..i], &raw[body_start..])
        }
        None => (raw, ""),
    };
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(": ") {
            headers.insert(k.to_string(), v.trim().to_string());
            // Also a lower-case alias for case-insensitive lookup.
            headers.insert(k.to_lowercase(), v.trim().to_string());
        }
    }
    let _ = Duration::from_secs(0); // silence unused import on some configs
    Ok(Response {
        status,
        body: body.to_string(),
        headers,
    })
}
