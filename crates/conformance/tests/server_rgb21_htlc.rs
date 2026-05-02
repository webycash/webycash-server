//! End-to-end HTLC swap on the RGB21 (collectible) server.
//!
//! Same primitive as `server_rgb_htlc_swap.rs` (which targets the fungible
//! flavor) but exercised on `webycash-server-rgb-collectible`. Drives the
//! server through the HTLC predicate paths defined in
//! `webycash-asset-rgb::htlc` and described in
//! `webycash-server/docs/referee-zkp-based-swap.md` §7 (HTLC swap on RGB21
//! — used by RGB21 ↔ Bitcoin ARK and RGB21 ↔ Webcash flows).
//!
//! Wire-format note: RGB21 has NO amount segment. Tokens are
//! `secret:{hex64}:{contract}:{issuer}` (no `e1.0:` prefix).
//!
//! Because RGB21 cannot mine, every test mints its starting NFT through
//! the operator-signed `/api/v1/issue` flow (Ed25519 signature over the
//! canonical body, registered via the `WEBYCASH_ISSUERS` env var).
//!
//! Scenarios covered:
//!
//! 1. Issue + lock + claim with the correct preimage (happy path).
//! 2. Lock-then-claim with the WRONG preimage rejects.
//! 3. Plain `/replace` of a locked record without an `htlc_witnesses`
//!    entry rejects.
//! 4. Lock-then-refund BEFORE timeout rejects.
//! 5. Lock-then-refund AFTER timeout accepts.

mod common;
use common::*;

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};

// ─────────────────────────────────────────────────────────────────────────────
// Local harness — server-rgb-collectible needs an issuer keypair registered
// at boot time. The shared `TestHarness` in `common::` only handles webcash
// and rgb-fungible (no /issue requirement), so this test crate brings its
// own thin spawner. Same shape as `server_rgb_collectible_lifecycle.rs`.
// ─────────────────────────────────────────────────────────────────────────────

struct CollectibleHarness {
    bind: String,
    sk: SigningKey,
    issuer: String,
    redis_name: String,
    child: Child,
}

impl CollectibleHarness {
    fn start(seed: u8) -> Option<Self> {
        let bin = bin_path("webycash-server-rgb-collectible");
        if !bin.exists() || !docker_avail() {
            eprintln!("skipping: binary or docker not available");
            return None;
        }

        let redis_port = ephemeral_port();
        let redis_name = format!("conf-rgb21-htlc-{seed:02x}-{}", short_id());
        Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &redis_name,
                "-p",
                &format!("{redis_port}:6379"),
                "redis:7-alpine",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !await_tcp("127.0.0.1", redis_port, Duration::from_secs(15)) {
            let _ = stop(&redis_name);
            return None;
        }

        let sk = SigningKey::from_bytes(&[seed; 32]);
        let vk = sk.verifying_key();
        let pubkey_hex = hex::encode(vk.as_bytes());
        let issuer = hex::encode(&vk.as_bytes()[..20]);

        let server_port = ephemeral_port();
        let bind = format!("127.0.0.1:{server_port}");
        let issuers_env = format!("{issuer}:{pubkey_hex}");

        let child = Command::new(&bin)
            .env("WEBCASH_BIND_ADDR", &bind)
            .env("WEBCASH_MODE", "testnet")
            .env("REDIS_URL", format!("redis://127.0.0.1:{redis_port}"))
            .env("WEBYCASH_ISSUERS", &issuers_env)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        if !await_tcp("127.0.0.1", server_port, Duration::from_secs(8)) {
            let mut harness = Self {
                bind,
                sk,
                issuer,
                redis_name,
                child,
            };
            let _ = harness.child.kill();
            let _ = harness.child.wait();
            let _ = stop(&harness.redis_name);
            return None;
        }

        Some(Self {
            bind,
            sk,
            issuer,
            redis_name,
            child,
        })
    }

    /// Mint a single NFT via `/api/v1/issue` — Ed25519-signed canonical body.
    fn issue(&self, contract: &str, secret_hex: &str, nonce: &str) {
        let body_obj = serde_json::json!({
            "issuer_fp": self.issuer,
            "outputs": [format!("secret:{secret_hex}:{contract}:{}", self.issuer)],
            "nonce": nonce,
            "ts": 1714003200_u64,
            "legalese": {"terms": true},
        });
        let body = serde_json::to_vec(&body_obj).expect("json");
        let sig = self.sk.sign(&body);
        let sig_hex = hex::encode(sig.to_bytes());
        let (status, resp) = post_with_header(
            &format!("http://{}/api/v1/issue", self.bind),
            std::str::from_utf8(&body).unwrap(),
            ("X-Issuer-Signature", &sig_hex),
        )
        .expect("issue");
        assert_eq!(status, 200, "issue failed: {resp}");
    }

    fn post_json(&self, path: &str, body: &serde_json::Value) -> (u16, String) {
        post(
            &format!("http://{}{}", self.bind, path),
            &serde_json::to_string(body).unwrap(),
        )
        .expect("post")
    }
}

impl Drop for CollectibleHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = stop(&self.redis_name);
    }
}

#[test]
fn rgb21_htlc_lock_and_claim_with_correct_preimage() {
    let Some(h) = CollectibleHarness::start(0x01) else {
        return;
    };
    let contract = format!("rgb21-htlc-claim-{}", short_id());
    let alice_in = "a".repeat(64);
    h.issue(&contract, &alice_in, "issue-claim-1");

    let locked_secret = "1".repeat(64);
    let claim_recipient_secret = "b".repeat(64);
    let refund_self_secret = "c".repeat(64);
    let x = "d".repeat(64);
    let h_hex = sha256_hex(&x);
    let issuer = &h.issuer;

    // Lock: 1:1 replace into HTLC state.
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{alice_in}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_locks": [{
                "output_index": 0,
                "request": {
                    "committed_h_hex": h_hex,
                    "refund_after_seconds_from_now": 3600,
                    "claim_owner_secret_hex": claim_recipient_secret,
                    "refund_owner_secret_hex": refund_self_secret,
                },
            }],
        }),
    );
    assert_eq!(status, 200, "lock failed: {body}");

    // Claim with the correct preimage. Output owner = Bob's claim secret.
    let bob_final = claim_recipient_secret.clone();
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{bob_final}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_witnesses": [{
                "input_index": 0,
                "witness": {
                    "provided_x_hex": x,
                    "output_owner_hash_hex": sha256_hex(&bob_final),
                },
            }],
        }),
    );
    assert_eq!(status, 200, "claim should succeed: {body}");

    // Post-conditions.
    let (_, body) = h.post_json(
        "/api/v1/health_check",
        &serde_json::json!([
            format!("public:{}:{contract}:{issuer}", sha256_hex(&locked_secret)),
            format!("public:{}:{contract}:{issuer}", sha256_hex(&bob_final)),
        ]),
    );
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
fn rgb21_htlc_claim_with_wrong_preimage_rejects() {
    let Some(h) = CollectibleHarness::start(0x02) else {
        return;
    };
    let contract = format!("rgb21-htlc-bad-{}", short_id());
    let alice_in = "2".repeat(64);
    h.issue(&contract, &alice_in, "issue-bad-1");

    let locked_secret = "3".repeat(64);
    let claim_recipient_secret = "4".repeat(64);
    let refund_self_secret = "5".repeat(64);
    let x = "6".repeat(64);
    let h_hex = sha256_hex(&x);
    let issuer = &h.issuer;

    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{alice_in}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_locks": [{
                "output_index": 0,
                "request": {
                    "committed_h_hex": h_hex,
                    "refund_after_seconds_from_now": 3600,
                    "claim_owner_secret_hex": claim_recipient_secret,
                    "refund_owner_secret_hex": refund_self_secret,
                },
            }],
        }),
    );
    assert_eq!(status, 200, "lock: {body}");

    // Wrong preimage — must reject.
    let wrong_x = "7".repeat(64);
    let bob_final = claim_recipient_secret.clone();
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{bob_final}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_witnesses": [{
                "input_index": 0,
                "witness": {
                    "provided_x_hex": wrong_x,
                    "output_owner_hash_hex": sha256_hex(&bob_final),
                },
            }],
        }),
    );
    assert_eq!(status, 500, "wrong preimage must reject: {body}");
}

#[test]
fn rgb21_htlc_replace_locked_without_witness_rejects() {
    let Some(h) = CollectibleHarness::start(0x03) else {
        return;
    };
    let contract = format!("rgb21-htlc-nowit-{}", short_id());
    let alice_in = "8".repeat(64);
    h.issue(&contract, &alice_in, "issue-nowit-1");

    let locked_secret = "9".repeat(64);
    let h_hex = sha256_hex(&"e".repeat(64));
    let issuer = &h.issuer;

    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{alice_in}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_locks": [{
                "output_index": 0,
                "request": {
                    "committed_h_hex": h_hex,
                    "refund_after_seconds_from_now": 3600,
                    "claim_owner_secret_hex": "a".repeat(64),
                    "refund_owner_secret_hex": "b".repeat(64),
                },
            }],
        }),
    );
    assert_eq!(status, 200, "lock: {body}");

    // Plain /replace of the HTLC-locked record (no witness) — must reject.
    let unrelated = "f".repeat(64);
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{unrelated}:{contract}:{issuer}")],
            "legalese": {"terms": true},
        }),
    );
    assert_eq!(status, 500, "missing witness must reject: {body}");
}

#[test]
fn rgb21_htlc_refund_before_timeout_rejects() {
    let Some(h) = CollectibleHarness::start(0x04) else {
        return;
    };
    let contract = format!("rgb21-htlc-refundlock-{}", short_id());
    let alice_in = "1".repeat(64);
    h.issue(&contract, &alice_in, "issue-refundlock-1");

    let locked_secret = "2".repeat(64);
    let claim_recipient_secret = "3".repeat(64);
    let refund_self_secret = "4".repeat(64);
    let x = "5".repeat(64);
    let h_hex = sha256_hex(&x);
    let issuer = &h.issuer;

    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{alice_in}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_locks": [{
                "output_index": 0,
                "request": {
                    "committed_h_hex": h_hex,
                    "refund_after_seconds_from_now": 3600,
                    "claim_owner_secret_hex": claim_recipient_secret,
                    "refund_owner_secret_hex": refund_self_secret,
                },
            }],
        }),
    );
    assert_eq!(status, 200, "lock: {body}");

    // Refund right away — no preimage; output owner = refund secret.
    let refund_final = refund_self_secret.clone();
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{refund_final}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_witnesses": [{
                "input_index": 0,
                "witness": {
                    "provided_x_hex": null,
                    "output_owner_hash_hex": sha256_hex(&refund_final),
                },
            }],
        }),
    );
    assert_eq!(status, 500, "refund before timeout must reject: {body}");
}

#[test]
fn rgb21_htlc_refund_after_timeout_accepts() {
    let Some(h) = CollectibleHarness::start(0x05) else {
        return;
    };
    let contract = format!("rgb21-htlc-refundok-{}", short_id());
    let alice_in = "6".repeat(64);
    h.issue(&contract, &alice_in, "issue-refundok-1");

    let locked_secret = "7".repeat(64);
    let claim_recipient_secret = "8".repeat(64);
    let refund_self_secret = "9".repeat(64);
    let x = "a".repeat(64);
    let h_hex = sha256_hex(&x);
    let issuer = &h.issuer;

    // 1-second lock so we can sleep past it.
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{alice_in}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_locks": [{
                "output_index": 0,
                "request": {
                    "committed_h_hex": h_hex,
                    "refund_after_seconds_from_now": 1,
                    "claim_owner_secret_hex": claim_recipient_secret,
                    "refund_owner_secret_hex": refund_self_secret,
                },
            }],
        }),
    );
    assert_eq!(status, 200, "lock: {body}");

    std::thread::sleep(Duration::from_secs(2));

    let refund_final = refund_self_secret.clone();
    let (status, body) = h.post_json(
        "/api/v1/replace",
        &serde_json::json!({
            "webcashes": [format!("secret:{locked_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{refund_final}:{contract}:{issuer}")],
            "legalese": {"terms": true},
            "htlc_witnesses": [{
                "input_index": 0,
                "witness": {
                    "provided_x_hex": null,
                    "output_owner_hash_hex": sha256_hex(&refund_final),
                },
            }],
        }),
    );
    assert_eq!(status, 200, "refund after timeout must accept: {body}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Local helpers (mirror the `server_rgb_collectible_lifecycle.rs` shape so
// each conformance test is fully self-contained — no extra wiring in
// common/mod.rs that other tests don't need).
// ─────────────────────────────────────────────────────────────────────────────

fn bin_path(b: &str) -> std::path::PathBuf {
    let m = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    std::path::PathBuf::from(&m)
        .ancestors()
        .nth(2)
        .unwrap()
        .join("target")
        .join("debug")
        .join(b)
}

fn ephemeral_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

fn docker_avail() -> bool {
    Command::new("docker")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn stop(name: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("docker")
        .args(["stop", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn short_id() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    )
}

fn await_tcp(host: &str, port: u16, max: Duration) -> bool {
    let deadline = std::time::Instant::now() + max;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    post_with_header(url, body, ("X-Test", ""))
}

fn post_with_header(url: &str, body: &str, extra: (&str, &str)) -> std::io::Result<(u16, String)> {
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".into()));
    let mut s = std::net::TcpStream::connect(&host_port)?;
    let extra_hdr = if extra.1.is_empty() {
        String::new()
    } else {
        format!("{}: {}\r\n", extra.0, extra.1)
    };
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        body.len(),
        extra_hdr,
    );
    s.write_all(req.as_bytes())?;
    s.write_all(body.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    Ok((status, text[body_start..].to_string()))
}
